// Futureboard Studio Status Bar (`shell_regions.rs` port).

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../engine/studio_controller.dart';
import '../theme/fb_tokens.dart';

class StatusBar extends StatelessWidget {
  final StudioController controller;

  const StatusBar({super.key, required this.controller});

  @override
  Widget build(BuildContext context) {
    final c = controller.colors;
    final tel = controller.telemetry;
    final p = controller.project;

    return Container(
      height: FbSize.statusbarH,
      padding: const EdgeInsets.symmetric(horizontal: FbSpace.s3),
      decoration: BoxDecoration(
        color: c.bgCanvas,
        border: Border(
          top: BorderSide(color: c.borderDefault, width: 1.0),
        ),
      ),
      child: Row(
        children: [
          Container(
            width: 7,
            height: 7,
            decoration: BoxDecoration(
              shape: BoxShape.circle,
              color: c.statusOk,
            ),
          ),
          const SizedBox(width: 6),
          Expanded(
            child: Text(
              controller.statusBanner,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: FbTypography.label(c),
            ),
          ),
          const SizedBox(width: FbSpace.s3),
          Icon(LucideIcons.cpu, size: 11, color: c.textMuted),
          const SizedBox(width: 4),
          Text(
            'DSP ${tel.cpuLoadPct.toStringAsFixed(1)}%',
            style: FbTypography.monoReadout(
              c,
              size: 10.5,
              color: c.textSecondary,
            ),
          ),
          const SizedBox(width: FbSpace.s3),
          Text(
            '${(p.sampleRate / 1000.0).toStringAsFixed(1)} kHz • 32f',
            style: FbTypography.monoReadout(
              c,
              size: 10.5,
              color: c.textMuted,
            ),
          ),
          const SizedBox(width: FbSpace.s3),
          Text(
            'LUFS-I ${tel.lufsIntegrated.toStringAsFixed(1)} | ${tel.truePeakDbtp.toStringAsFixed(1)} dBTP',
            style: FbTypography.monoReadout(
              c,
              size: 10.5,
              color: c.accentPrimary,
            ),
          ),
        ],
      ),
    );
  }
}
