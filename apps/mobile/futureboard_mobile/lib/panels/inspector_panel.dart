// Futureboard Studio Inspector Panel: Track Strip, Insert Rack & MIT KEMAR 3D Spatial Audio Panner.

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../engine/studio_controller.dart';
import '../theme/fb_tokens.dart';
import '../widgets/fb_controls.dart';

class InspectorPanel extends StatelessWidget {
  final StudioController controller;

  const InspectorPanel({super.key, required this.controller});

  @override
  Widget build(BuildContext context) {
    final c = controller.colors;
    final track = controller.selectedTrack;
    final trackColor = FbColors.parseHex(track.colorHex, c.accentPrimary);

    return Container(
      width: 248,
      decoration: BoxDecoration(
        color: c.bgPrimary,
        border: Border(
          left: BorderSide(color: c.borderDefault, width: 1.0),
        ),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          // Header
          Container(
            height: 32,
            padding: const EdgeInsets.symmetric(horizontal: FbSpace.s3),
            decoration: BoxDecoration(
              color: c.bgSecondary,
              border: Border(
                bottom: BorderSide(color: c.borderSubtle, width: 1.0),
              ),
            ),
            child: Row(
              children: [
                Container(width: 8, height: 8, color: trackColor),
                const SizedBox(width: 6),
                Expanded(
                  child: Text(
                    'INSPECTOR • ${track.name.toUpperCase()}',
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: FbTypography.sectionHeader(c),
                  ),
                ),
                if (track.id != 'master' &&
                    controller.project.tracks.length > 1)
                  FbIconButton(
                    icon: LucideIcons.trash2,
                    tooltip: 'Delete Selected Track',
                    size: 22,
                    colors: c,
                    onTap: () => controller.deleteTrack(track.id),
                  ),
              ],
            ),
          ),

          Expanded(
            child: SingleChildScrollView(
              padding: const EdgeInsets.all(FbSpace.s3),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  // 1. MIT KEMAR Binaural 3D Room Panner (`solfege_spatialaudio`)
                  Row(
                    children: [
                      Text('3D SPATIAL ROOM', style: FbTypography.sectionHeader(c)),
                      const Spacer(),
                      FbLatchButton(
                        label: '3D',
                        tooltip: 'Enable MIT KEMAR Binaural HRTF on Track',
                        active: track.spatialBinaural ||
                            controller.project.masterBinauralEnabled,
                        latchColor: c.stateAuto,
                        colors: c,
                        size: 20,
                        onTap: () => controller.setTrackSpatial(
                          track.id,
                          x: track.spatialX,
                          y: track.spatialY,
                          z: track.spatialZ,
                          binaural: !track.spatialBinaural,
                        ),
                      ),
                    ],
                  ),
                  const SizedBox(height: 6),
                  GestureDetector(
                    onPanUpdate: (d) {
                      final nx = ((d.localPosition.dx / 220.0) * 2.0 - 1.0)
                          .clamp(-1.0, 1.0);
                      final ny = (1.0 - (d.localPosition.dy / 130.0) * 2.0)
                          .clamp(-1.0, 1.0);
                      controller.setTrackSpatial(
                        track.id,
                        x: nx,
                        y: ny,
                        z: track.spatialZ,
                        binaural: true,
                      );
                    },
                    child: Container(
                      height: 130,
                      decoration: BoxDecoration(
                        color: c.bgSunken,
                        border: Border.all(color: c.borderDefault),
                      ),
                      child: CustomPaint(
                        painter: _SpatialRoomPadPainter(
                          x: track.spatialX,
                          y: track.spatialY,
                          color: trackColor,
                          colors: c,
                        ),
                      ),
                    ),
                  ),
                  const SizedBox(height: 4),
                  Row(
                    children: [
                      Expanded(
                        child: Text(
                          'X: ${track.spatialX.toStringAsFixed(2)}  Y: ${track.spatialY.toStringAsFixed(2)}',
                          maxLines: 1,
                          overflow: TextOverflow.ellipsis,
                          style: FbTypography.monoReadout(c, size: 10),
                        ),
                      ),
                      Text(
                        'KEMAR HRTF',
                        style: FbTypography.micro(c),
                      ),
                    ],
                  ),
                  const SizedBox(height: FbSpace.s4),

                  // 2. Track Instrument & Insert FX Rack
                  Text('SIGNAL CHAIN', style: FbTypography.sectionHeader(c)),
                  const SizedBox(height: 6),
                  if (track.instrument != null)
                    _chainRow(
                      title: 'INST: ${track.instrument!.name}',
                      enabled: track.instrument!.enabled,
                      onOpen: () => controller.inspectPlugin(track.id, null),
                      onToggle: () => controller.togglePluginEnabled(
                          track.id, null, !track.instrument!.enabled),
                      onRemove: null,
                      c: c,
                    ),
                  for (final ins in track.inserts)
                    Padding(
                      padding: const EdgeInsets.only(top: 4),
                      child: _chainRow(
                        title: 'FX ${ins.slotIndex + 1}: ${ins.name}',
                        enabled: ins.enabled,
                        onOpen: () =>
                            controller.inspectPlugin(track.id, ins.slotIndex),
                        onToggle: () => controller.togglePluginEnabled(
                            track.id, ins.slotIndex, !ins.enabled),
                        onRemove: () =>
                            controller.removeTrackInsert(track.id, ins.slotIndex),
                        c: c,
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

  Widget _chainRow({
    required String title,
    required bool enabled,
    required VoidCallback onOpen,
    required VoidCallback onToggle,
    required VoidCallback? onRemove,
    required FbColors c,
  }) {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 4),
      decoration: BoxDecoration(
        color: c.bgSecondary,
        borderRadius: BorderRadius.circular(FbRadius.sm),
        border: Border.all(color: c.borderSubtle),
      ),
      child: Row(
        children: [
          FbLatchButton(
            label: 'ON',
            tooltip: 'Enable/Bypass',
            active: enabled,
            latchColor: c.statusOk,
            colors: c,
            size: 18,
            onTap: onToggle,
          ),
          const SizedBox(width: 6),
          Expanded(
            child: InkWell(
              onTap: onOpen,
              child: Text(
                title,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: FbTypography.body(c),
              ),
            ),
          ),
          if (onRemove != null)
            InkWell(
              onTap: onRemove,
              child: Icon(LucideIcons.x, size: 13, color: c.textMuted),
            ),
        ],
      ),
    );
  }
}

class _SpatialRoomPadPainter extends CustomPainter {
  final double x;
  final double y;
  final Color color;
  final FbColors colors;

  _SpatialRoomPadPainter({
    required this.x,
    required this.y,
    required this.color,
    required this.colors,
  });

  @override
  void paint(Canvas canvas, Size size) {
    final cx = size.width / 2;
    final cy = size.height / 2;
    final gridPaint = Paint()
      ..color = colors.borderSubtle
      ..style = PaintingStyle.stroke
      ..strokeWidth = 1.0;

    canvas.drawLine(Offset(cx, 0), Offset(cx, size.height), gridPaint);
    canvas.drawLine(Offset(0, cy), Offset(size.width, cy), gridPaint);
    canvas.drawCircle(Offset(cx, cy), size.height * 0.35, gridPaint);

    // Listener head icon at center
    canvas.drawCircle(
      Offset(cx, cy),
      5.0,
      Paint()..color = colors.textMuted,
    );

    // Source position dot
    final sx = ((x + 1.0) * 0.5) * size.width;
    final sy = ((1.0 - y) * 0.5) * size.height;
    canvas.drawCircle(
      Offset(sx, sy),
      8.0,
      Paint()..color = color.withValues(alpha: 0.30),
    );
    canvas.drawCircle(
      Offset(sx, sy),
      4.5,
      Paint()..color = color,
    );
  }

  @override
  bool shouldRepaint(covariant _SpatialRoomPadPainter oldDelegate) =>
      oldDelegate.x != x || oldDelegate.y != y;
}
