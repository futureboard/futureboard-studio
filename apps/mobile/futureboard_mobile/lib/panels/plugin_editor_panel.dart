// Native Built-In Plugin Editors for all 27 Rust DSP Plugins (`plugin_editor_panel.dart`).

import 'dart:math' as math;
import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../engine/studio_controller.dart';
import '../models/studio_models.dart';
import '../theme/fb_tokens.dart';
import '../widgets/fb_controls.dart';

class PluginEditorPanel extends StatelessWidget {
  final StudioController controller;

  const PluginEditorPanel({super.key, required this.controller});

  @override
  Widget build(BuildContext context) {
    final c = controller.colors;
    final track = controller.inspectedPluginTrack;
    final slot = controller.inspectedPluginSlot;

    if (slot == null) {
      return Container(
        color: c.bgCanvas,
        alignment: Alignment.center,
        child: Text(
          'Select an Instrument or Effect Insert to open its Native Rust DSP Editor',
          style: FbTypography.body(c),
        ),
      );
    }

    final slotIndex = controller.inspectedPluginSlotIndex;
    final pluginMeter = controller.telemetry.inspectedPluginMeter;
    final catalogEntry = controller.catalog
        .where((e) => e.id == slot.pluginId)
        .firstOrNull;

    return Container(
      color: c.bgCanvas,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          // ── Plugin Header Bar: Track Slot Selector, Bypass, Presets ──
          Container(
            height: 32,
            padding: const EdgeInsets.symmetric(horizontal: FbSpace.s3),
            decoration: BoxDecoration(
              color: c.bgSecondary,
              border: Border(
                bottom: BorderSide(color: c.borderDefault, width: 1.0),
              ),
            ),
            child: SingleChildScrollView(
              scrollDirection: Axis.horizontal,
              child: Row(
                children: [
                  // Power / Bypass Toggle
                  FbLatchButton(
                    label: 'ON',
                    tooltip: 'Enable / Bypass Plugin',
                    active: slot.enabled,
                    latchColor: c.statusOk,
                    colors: c,
                    size: 22,
                    onTap: () => controller.togglePluginEnabled(
                        track.id, slotIndex, !slot.enabled),
                  ),
                  const SizedBox(width: 8),
                  Text(
                    '${track.name.toUpperCase()} • ${slot.name.toUpperCase()}',
                    style: FbTypography.sectionHeader(c),
                  ),
                  const SizedBox(width: FbSpace.s3),

                  // Quick slot pills for this track
                  if (track.instrument != null)
                    Padding(
                      padding: const EdgeInsets.only(right: 4),
                      child: FbButton(
                        label: track.instrument!.name.split(' ').first,
                        compact: true,
                        active: slotIndex == null,
                        activeColor: const Color(0xFF9B6BFF),
                        colors: c,
                        onTap: () => controller.inspectPlugin(track.id, null),
                      ),
                    ),
                  for (final ins in track.inserts)
                    Padding(
                      padding: const EdgeInsets.only(right: 4),
                      child: FbButton(
                        label: 'FX${ins.slotIndex + 1}: ${ins.name.split(' ').first}',
                        compact: true,
                        active: slotIndex == ins.slotIndex,
                        colors: c,
                        onTap: () =>
                            controller.inspectPlugin(track.id, ins.slotIndex),
                      ),
                    ),

                  const SizedBox(width: FbSpace.s3),
                  // Factory Presets Menu
                  if (catalogEntry != null && catalogEntry.presets.isNotEmpty)
                    PopupMenuButton<String>(
                      tooltip: 'Factory Presets',
                      color: c.bgElevated,
                      onSelected: (preset) => controller.applyPluginPreset(
                          track.id, slotIndex, preset),
                      itemBuilder: (context) => [
                        for (final pName in catalogEntry.presets)
                          PopupMenuItem(
                            value: pName,
                            height: 28,
                            child: Text(pName, style: FbTypography.body(c)),
                          ),
                      ],
                      child: Container(
                        height: 22,
                        padding: const EdgeInsets.symmetric(horizontal: 8),
                        alignment: Alignment.center,
                        decoration: BoxDecoration(
                          color: c.bgSunken,
                          borderRadius: BorderRadius.circular(FbRadius.xs),
                          border: Border.all(color: c.borderDefault),
                        ),
                        child: Row(
                          children: [
                            Icon(LucideIcons.slidersHorizontal,
                                size: 11, color: c.accentPrimary),
                            const SizedBox(width: 5),
                            Text(
                              'Preset: ${slot.activePreset}',
                              style: FbTypography.monoReadout(c, size: 10),
                            ),
                          ],
                        ),
                      ),
                    ),
                ],
              ),
            ),
          ),

          // ── Main Split: Custom Interactive Visualizer Left + Parameter Knobs Right
          Expanded(
            child: Padding(
              padding: const EdgeInsets.all(FbSpace.s3),
              child: Row(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  // Left Custom Visualizer for this plugin family
                  SizedBox(
                    width: 280,
                    child: _buildCustomVisualizer(
                      track: track,
                      slotIndex: slotIndex,
                      slot: slot,
                      meter: pluginMeter,
                      c: c,
                    ),
                  ),
                  const SizedBox(width: FbSpace.s3),

                  // Right Parameter Groups & Rotary Knobs
                  Expanded(
                    child: Container(
                      padding: const EdgeInsets.all(FbSpace.s3),
                      decoration: BoxDecoration(
                        color: c.bgPrimary,
                        borderRadius: BorderRadius.circular(FbRadius.sm),
                        border: Border.all(color: c.borderDefault),
                      ),
                      child: SingleChildScrollView(
                        child: Wrap(
                          spacing: 14,
                          runSpacing: 12,
                          children: [
                            for (final param in slot.params)
                              if (param.unit == 'bool')
                                _buildBoolToggleParam(
                                    track.id, slotIndex, param, c)
                              else
                                SizedBox(
                                  width: 66,
                                  child: FbRotaryKnob(
                                    label: param.name,
                                    value: param.value,
                                    min: param.min,
                                    max: param.max,
                                    defaultValue: param.defaultValue,
                                    unit: param.unit,
                                    colors: c,
                                    diameter: 38,
                                    onChanged: (v) => controller.setPluginParam(
                                      track.id,
                                      slotIndex,
                                      param.id,
                                      v,
                                    ),
                                  ),
                                ),
                          ],
                        ),
                      ),
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

  Widget _buildBoolToggleParam(
    String trackId,
    int? slotIndex,
    PluginParamModel param,
    FbColors c,
  ) {
    final active = param.value >= 0.5;
    return SizedBox(
      width: 66,
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          FbLatchButton(
            label: active ? 'ON' : 'OFF',
            tooltip: param.name,
            active: active,
            latchColor: c.accentPrimary,
            colors: c,
            size: 32,
            onTap: () => controller.setPluginParam(
              trackId,
              slotIndex,
              param.id,
              active ? 0.0 : 1.0,
            ),
          ),
          const SizedBox(height: 4),
          Text(
            param.name,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: FbTypography.micro(c),
          ),
        ],
      ),
    );
  }

  Widget _buildCustomVisualizer({
    required TrackModel track,
    required int? slotIndex,
    required InsertSlotModel slot,
    required PluginMeterModel? meter,
    required FbColors c,
  }) {
    final pid = slot.pluginId;

    // 1. 16-Pad Drum Sampler MPC Grid
    if (pid == 'drumsampler') {
      return _buildDrumSamplerGrid(track.id, meter, c);
    }

    // 2. Interactive Parametric EQ Curve + Live 128-Bin Spectrum (`equz8`, `equzx`, `sphere.eq8`, `c1073`)
    if (pid == 'equz8' ||
        pid == 'equzx' ||
        pid == 'sphere.eq8' ||
        pid == 'c1073') {
      return GestureDetector(
        onPanUpdate: (d) {
          // Dragging inside the EQ canvas sweeps Band 5 (Presence) or Mid band
          final freqNorm = (d.localPosition.dx / 280.0).clamp(0.0, 1.0);
          final freq = 20.0 * math.pow(1000.0, freqNorm);
          final gain = ((0.5 - (d.localPosition.dy / 160.0)) * 36.0)
              .clamp(-18.0, 18.0);
          if (pid == 'c1073') {
            controller.setPluginParam(
                track.id, slotIndex, 'mid_freq_hz', freq.toDouble());
            controller.setPluginParam(track.id, slotIndex, 'mid_gain_db', gain);
          } else {
            controller.setPluginParam(
                track.id, slotIndex, 'band5_freq_hz', freq.toDouble());
            controller.setPluginParam(
                track.id, slotIndex, 'band5_gain_db', gain);
          }
        },
        child: Container(
          decoration: BoxDecoration(
            color: c.bgSunken,
            border: Border.all(color: c.borderDefault),
          ),
          child: CustomPaint(
            painter: _EqAnalyzerPainter(
              eqCurveDb: meter?.eqCurveDb ?? const [],
              spectrumDb: controller.telemetry.spectrumDb,
              colors: c,
            ),
          ),
        ),
      );
    }

    // 3. Dynamics Transfer Curve & Gain Reduction Visualizer
    return Container(
      padding: const EdgeInsets.all(8),
      decoration: BoxDecoration(
        color: c.bgSunken,
        border: Border.all(color: c.borderDefault),
      ),
      child: Row(
        children: [
          Expanded(
            child: CustomPaint(
              size: const Size(double.infinity, double.infinity),
              painter: _DynamicsVisualizerPainter(
                slot: slot,
                grDb: meter?.gainReductionDb ?? 0.0,
                colors: c,
              ),
            ),
          ),
          const SizedBox(width: 8),
          FbStereoMeter(
            peakLDb: meter?.inputPeakDb ?? -96.0,
            peakRDb: meter?.outputPeakDb ?? -96.0,
            gainReductionDb: meter?.gainReductionDb ?? 0.0,
            colors: c,
            width: 22,
          ),
        ],
      ),
    );
  }

  Widget _buildDrumSamplerGrid(
    String trackId,
    PluginMeterModel? meter,
    FbColors c,
  ) {
    const padNames = [
      'Kick 808',
      'Kick Ac',
      'Snare',
      'Clap',
      'Rimshot',
      'Closed HH',
      'Pedal HH',
      'Open HH',
      'Tom Low',
      'Tom Mid',
      'Tom High',
      'Crash',
      'Ride',
      'Cowbell',
      'Shaker',
      'FM Perc',
    ];
    final padLevels = meter?.padLevels ?? const [];

    return Container(
      padding: const EdgeInsets.all(6),
      decoration: BoxDecoration(
        color: c.bgSunken,
        border: Border.all(color: c.borderDefault),
      ),
      child: GridView.builder(
        itemCount: 16,
        gridDelegate: const SliverGridDelegateWithFixedCrossAxisCount(
          crossAxisCount: 4,
          mainAxisSpacing: 4,
          crossAxisSpacing: 4,
          childAspectRatio: 1.45,
        ),
        itemBuilder: (context, idx) {
          final midiNote = 36 + idx;
          final level = idx < padLevels.length ? padLevels[idx] : 0.0;
          final active = level > 0.05;

          return GestureDetector(
            onTapDown: (_) => controller.triggerLiveMidi(midiNote, 118, true,
                trackId: trackId),
            onTapUp: (_) => controller.triggerLiveMidi(midiNote, 0, false,
                trackId: trackId),
            onTapCancel: () => controller.triggerLiveMidi(midiNote, 0, false,
                trackId: trackId),
            child: Container(
              padding: const EdgeInsets.all(4),
              decoration: BoxDecoration(
                color: active
                    ? c.stateMute.withValues(alpha: 0.35)
                    : c.bgSecondary,
                borderRadius: BorderRadius.circular(FbRadius.xs),
                border: Border.all(
                  color: active ? c.stateMute : c.borderDefault,
                  width: active ? 1.5 : 1.0,
                ),
              ),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                mainAxisAlignment: MainAxisAlignment.spaceBetween,
                children: [
                  Text(
                    'P${idx + 1}',
                    style: FbTypography.monoReadout(
                      c,
                      size: 8.5,
                      color: active ? c.stateMute : c.textMuted,
                    ),
                  ),
                  Text(
                    padNames[idx],
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: TextStyle(
                      fontFamily: FbTypography.fontSans,
                      fontSize: 9.5,
                      fontWeight: FontWeight.w700,
                      color: c.textPrimary,
                    ),
                  ),
                ],
              ),
            ),
          );
        },
      ),
    );
  }
}

class _EqAnalyzerPainter extends CustomPainter {
  final List<double> eqCurveDb;
  final List<double> spectrumDb;
  final FbColors colors;

  _EqAnalyzerPainter({
    required this.eqCurveDb,
    required this.spectrumDb,
    required this.colors,
  });

  @override
  void paint(Canvas canvas, Size size) {
    final midY = size.height / 2.0;

    // dB Gridlines (-12, 0, +12 dB)
    for (final factor in [0.25, 0.5, 0.75]) {
      final y = size.height * factor;
      canvas.drawLine(
        Offset(0, y),
        Offset(size.width, y),
        Paint()
          ..color = factor == 0.5 ? colors.borderDefault : colors.gridSubLine
          ..strokeWidth = 1.0,
      );
    }

    // Live 128-bin FFT Spectrum background fill
    if (spectrumDb.isNotEmpty) {
      final specPath = Path()..moveTo(0, size.height);
      for (int i = 0; i < spectrumDb.length; i++) {
        final x = (i / (spectrumDb.length - 1)) * size.width;
        final norm = ((spectrumDb[i] + 90.0) / 90.0).clamp(0.0, 1.0);
        specPath.lineTo(x, size.height * (1.0 - norm));
      }
      specPath.lineTo(size.width, size.height);
      specPath.close();
      canvas.drawPath(
        specPath,
        Paint()..color = colors.accentPrimary.withValues(alpha: 0.18),
      );
    }

    // Composite EQ magnitude response curve
    if (eqCurveDb.isNotEmpty) {
      final eqPath = Path();
      for (int i = 0; i < eqCurveDb.length; i++) {
        final x = (i / (eqCurveDb.length - 1)) * size.width;
        final normDb = (eqCurveDb[i] / 24.0).clamp(-1.0, 1.0);
        final y = midY - normDb * (size.height * 0.42);
        if (i == 0) {
          eqPath.moveTo(x, y);
        } else {
          eqPath.lineTo(x, y);
        }
      }
      canvas.drawPath(
        eqPath,
        Paint()
          ..color = colors.meterGreen
          ..style = PaintingStyle.stroke
          ..strokeWidth = 2.0,
      );
    }
  }

  @override
  bool shouldRepaint(covariant _EqAnalyzerPainter oldDelegate) => true;
}

class _DynamicsVisualizerPainter extends CustomPainter {
  final InsertSlotModel slot;
  final double grDb;
  final FbColors colors;

  _DynamicsVisualizerPainter({
    required this.slot,
    required this.grDb,
    required this.colors,
  });

  @override
  void paint(Canvas canvas, Size size) {
    // 1:1 unity reference diagonal
    canvas.drawLine(
      Offset(0, size.height),
      Offset(size.width, 0),
      Paint()
        ..color = colors.borderDefault
        ..strokeWidth = 1.0,
    );

    final threshDb = slot.paramValue('threshold_db', -18.0);
    final ratio = slot.paramValue('ratio', 4.0).clamp(1.0, 20.0);
    final threshNorm = ((threshDb + 60.0) / 60.0).clamp(0.1, 0.95);

    final curve = Path()..moveTo(0, size.height);
    final kneeX = size.width * threshNorm;
    final kneeY = size.height * (1.0 - threshNorm);
    curve.lineTo(kneeX, kneeY);

    final remNorm = 1.0 - threshNorm;
    final outTopNorm = (threshNorm + remNorm / ratio).clamp(0.0, 1.0);
    curve.lineTo(size.width, size.height * (1.0 - outTopNorm));

    canvas.drawPath(
      curve,
      Paint()
        ..color = colors.accentPrimary
        ..style = PaintingStyle.stroke
        ..strokeWidth = 2.2,
    );

    if (grDb > 0.1) {
      final tp = TextPainter(
        text: TextSpan(
          text: 'GR -${grDb.toStringAsFixed(1)} dB',
          style: FbTypography.monoReadout(
            colors,
            size: 10.5,
            color: colors.meterOrange,
          ),
        ),
        textDirection: TextDirection.ltr,
      )..layout();
      tp.paint(canvas, const Offset(6, 6));
    }
  }

  @override
  bool shouldRepaint(covariant _DynamicsVisualizerPainter oldDelegate) => true;
}
