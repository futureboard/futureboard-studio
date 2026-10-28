// Shared Futureboard Studio UI controls ported from `crates/SphereUIComponents/src/components/`
// and adhering strictly to `DESIGN.md` tokens and two-channel state encoding.

import 'dart:math' as math;
import 'package:flutter/material.dart';
import '../theme/fb_tokens.dart';

/// Grouped cluster pill for titlebar and transport controls (`FbRadius.md`).
class FbChromeCluster extends StatelessWidget {
  final FbColors colors;
  final List<Widget> children;
  final EdgeInsetsGeometry padding;

  const FbChromeCluster({
    super.key,
    required this.colors,
    required this.children,
    this.padding = const EdgeInsets.symmetric(horizontal: 4, vertical: 2),
  });

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: padding,
      decoration: BoxDecoration(
        color: colors.bgSecondary,
        borderRadius: BorderRadius.circular(FbRadius.md),
        border: Border.all(color: colors.borderSubtle, width: 1.0),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: children,
      ),
    );
  }
}

/// Two-channel latched channel state button (`M`, `S`, `R`, `I`, `A`).
/// Encodes active state via both tinted fill wash (`latchFill`), strong border
/// (`latchBorder`), and high-contrast glyph (`DESIGN.md` -> "Two-channel latched states").
class FbLatchButton extends StatelessWidget {
  final String label;
  final String tooltip;
  final bool active;
  final Color latchColor;
  final FbColors colors;
  final VoidCallback onTap;
  final double size;

  const FbLatchButton({
    super.key,
    required this.label,
    required this.tooltip,
    required this.active,
    required this.latchColor,
    required this.colors,
    required this.onTap,
    this.size = FbSize.latchButton,
  });

  @override
  Widget build(BuildContext context) {
    final bg = active
        ? latchColor.withValues(alpha: FbState.latchFill)
        : colors.bgSunken;
    final border = active
        ? latchColor.withValues(alpha: FbState.latchBorder)
        : colors.borderDefault;
    final fg = active ? latchColor : colors.textSecondary;

    return Tooltip(
      message: tooltip,
      child: InkWell(
        onTap: onTap,
        borderRadius: BorderRadius.circular(FbRadius.xs),
        child: Container(
          width: size,
          height: size,
          alignment: Alignment.center,
          decoration: BoxDecoration(
            color: bg,
            borderRadius: BorderRadius.circular(FbRadius.xs),
            border: Border.all(color: border, width: active ? 1.3 : 1.0),
          ),
          child: Text(
            label,
            style: TextStyle(
              fontFamily: FbTypography.fontMono,
              fontSize: size * 0.48,
              fontWeight: active ? FontWeight.w800 : FontWeight.w600,
              color: fg,
              height: 1.0,
            ),
          ),
        ),
      ),
    );
  }
}

/// Compact studio action button with optional icon and active highlight.
class FbButton extends StatelessWidget {
  final String label;
  final IconData? icon;
  final bool active;
  final Color? activeColor;
  final FbColors colors;
  final VoidCallback? onTap;
  final bool compact;

  const FbButton({
    super.key,
    required this.label,
    this.icon,
    this.active = false,
    this.activeColor,
    required this.colors,
    this.onTap,
    this.compact = false,
  });

  @override
  Widget build(BuildContext context) {
    final accent = activeColor ?? colors.accentPrimary;
    final bg = active
        ? accent.withValues(alpha: FbState.latchFill)
        : colors.bgTertiary;
    final border = active
        ? accent.withValues(alpha: FbState.latchBorder)
        : colors.borderDefault;
    final fg = onTap == null
        ? colors.textMuted
        : (active ? accent : colors.textPrimary);

    return InkWell(
      onTap: onTap,
      borderRadius: BorderRadius.circular(FbRadius.sm),
      child: Container(
        height: compact ? FbSize.controlSm : FbSize.controlMd,
        padding: EdgeInsets.symmetric(
          horizontal: compact ? FbSpace.s2 + 2 : FbSpace.s3,
        ),
        decoration: BoxDecoration(
          color: bg,
          borderRadius: BorderRadius.circular(FbRadius.sm),
          border: Border.all(color: border, width: 1.0),
        ),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            if (icon != null) ...[
              Icon(icon, size: compact ? 12 : 14, color: fg),
              const SizedBox(width: 4),
            ],
            Text(
              label,
              style: TextStyle(
                fontFamily: FbTypography.fontSans,
                fontSize: compact ? 10.5 : 11.5,
                fontWeight: active ? FontWeight.w700 : FontWeight.w600,
                color: fg,
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// Square 28x28 studio icon button.
class FbIconButton extends StatelessWidget {
  final IconData icon;
  final String tooltip;
  final bool active;
  final Color? activeColor;
  final FbColors colors;
  final VoidCallback? onTap;
  final double size;

  const FbIconButton({
    super.key,
    required this.icon,
    required this.tooltip,
    this.active = false,
    this.activeColor,
    required this.colors,
    this.onTap,
    this.size = FbSize.iconButton,
  });

  @override
  Widget build(BuildContext context) {
    final accent = activeColor ?? colors.accentPrimary;
    final bg = active
        ? accent.withValues(alpha: FbState.latchFill)
        : Colors.transparent;
    final border = active
        ? accent.withValues(alpha: FbState.latchBorder)
        : Colors.transparent;
    final fg = onTap == null
        ? colors.textMuted.withValues(alpha: FbState.disabled)
        : (active ? accent : colors.textSecondary);

    return Tooltip(
      message: tooltip,
      child: InkWell(
        onTap: onTap,
        borderRadius: BorderRadius.circular(FbRadius.sm),
        child: Container(
          width: size,
          height: size,
          alignment: Alignment.center,
          decoration: BoxDecoration(
            color: bg,
            borderRadius: BorderRadius.circular(FbRadius.sm),
            border: Border.all(color: border, width: 1.0),
          ),
          child: Icon(icon, size: size * 0.56, color: fg),
        ),
      ),
    );
  }
}

/// Precision rotary parameter knob with vertical drag, double-tap to reset,
/// value arc, and tabular readout.
class FbRotaryKnob extends StatelessWidget {
  final String label;
  final double value;
  final double min;
  final double max;
  final double defaultValue;
  final String unit;
  final FbColors colors;
  final Color? accentColor;
  final ValueChanged<double> onChanged;
  final double diameter;

  const FbRotaryKnob({
    super.key,
    required this.label,
    required this.value,
    required this.min,
    required this.max,
    required this.defaultValue,
    this.unit = '',
    required this.colors,
    this.accentColor,
    required this.onChanged,
    this.diameter = 40.0,
  });

  double get _normalized =>
      ((value - min) / (max - min == 0 ? 1.0 : (max - min))).clamp(0.0, 1.0);

  String _formatValue() {
    if (unit == 'bool') return value >= 0.5 ? 'ON' : 'OFF';
    if (max - min >= 1000 && value.abs() >= 1000) {
      return '${(value / 1000.0).toStringAsFixed(2)}k$unit';
    }
    if (max - min >= 100) {
      return '${value.toStringAsFixed(0)}$unit';
    }
    return '${value.toStringAsFixed(1)}$unit';
  }

  @override
  Widget build(BuildContext context) {
    final accent = accentColor ?? colors.accentPrimary;
    return GestureDetector(
      onDoubleTap: () => onChanged(defaultValue),
      onVerticalDragUpdate: (details) {
        final span = (max - min).abs();
        final delta = (-details.delta.dy / 110.0) * span;
        onChanged((value + delta).clamp(min, max));
      },
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          SizedBox(
            width: diameter,
            height: diameter,
            child: CustomPaint(
              painter: _RotaryKnobPainter(
                normalized: _normalized,
                bipolar: min < 0 && max > 0 && (min + max).abs() < 0.01,
                accent: accent,
                colors: colors,
              ),
            ),
          ),
          const SizedBox(height: 2),
          Text(
            label,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: FbTypography.micro(colors),
          ),
          Text(
            _formatValue(),
            maxLines: 1,
            style: FbTypography.monoReadout(
              colors,
              size: 9.5,
              color: colors.textSecondary,
            ),
          ),
        ],
      ),
    );
  }
}

class _RotaryKnobPainter extends CustomPainter {
  final double normalized;
  final bool bipolar;
  final Color accent;
  final FbColors colors;

  _RotaryKnobPainter({
    required this.normalized,
    required this.bipolar,
    required this.accent,
    required this.colors,
  });

  @override
  void paint(Canvas canvas, Size size) {
    final center = Offset(size.width / 2, size.height / 2);
    final radius = size.width * 0.40;
    const startAngle = math.pi * 0.75;
    const sweepTotal = math.pi * 1.5;

    final trackPaint = Paint()
      ..color = colors.bgSunken
      ..style = PaintingStyle.stroke
      ..strokeWidth = 3.0
      ..strokeCap = StrokeCap.round;

    canvas.drawArc(
      Rect.fromCircle(center: center, radius: radius),
      startAngle,
      sweepTotal,
      false,
      trackPaint,
    );

    final arcPaint = Paint()
      ..color = accent
      ..style = PaintingStyle.stroke
      ..strokeWidth = 3.0
      ..strokeCap = StrokeCap.round;

    if (bipolar) {
      final midAngle = startAngle + sweepTotal * 0.5;
      final sweep = (normalized - 0.5) * sweepTotal;
      canvas.drawArc(
        Rect.fromCircle(center: center, radius: radius),
        midAngle,
        sweep,
        false,
        arcPaint,
      );
    } else {
      canvas.drawArc(
        Rect.fromCircle(center: center, radius: radius),
        startAngle,
        sweepTotal * normalized,
        false,
        arcPaint,
      );
    }

    // Cap body
    final bodyRadius = radius - 4.0;
    canvas.drawCircle(
      center,
      bodyRadius,
      Paint()..color = colors.bgElevated,
    );
    canvas.drawCircle(
      center,
      bodyRadius,
      Paint()
        ..color = colors.borderDefault
        ..style = PaintingStyle.stroke
        ..strokeWidth = 1.0,
    );

    // Pointer tick
    final angle = startAngle + sweepTotal * normalized;
    final p1 = Offset(
      center.dx + math.cos(angle) * (bodyRadius * 0.35),
      center.dy + math.sin(angle) * (bodyRadius * 0.35),
    );
    final p2 = Offset(
      center.dx + math.cos(angle) * (bodyRadius * 0.88),
      center.dy + math.sin(angle) * (bodyRadius * 0.88),
    );
    canvas.drawLine(
      p1,
      p2,
      Paint()
        ..color = colors.textPrimary
        ..strokeWidth = 1.8
        ..strokeCap = StrokeCap.round,
    );
  }

  @override
  bool shouldRepaint(covariant _RotaryKnobPainter oldDelegate) =>
      oldDelegate.normalized != normalized || oldDelegate.accent != accent;
}

/// Square-cornered (`FbRadius.none`) stereo Peak + RMS + Gain Reduction meter bar.
class FbStereoMeter extends StatelessWidget {
  final double peakLDb;
  final double peakRDb;
  final double rmsLDb;
  final double rmsRDb;
  final double gainReductionDb;
  final FbColors colors;
  final double width;
  final bool horizontal;

  const FbStereoMeter({
    super.key,
    required this.peakLDb,
    required this.peakRDb,
    this.rmsLDb = -96.0,
    this.rmsRDb = -96.0,
    this.gainReductionDb = 0.0,
    required this.colors,
    this.width = 14.0,
    this.horizontal = false,
  });

  @override
  Widget build(BuildContext context) {
    return CustomPaint(
      size: horizontal ? Size(width, 12.0) : Size(width, double.infinity),
      painter: _StereoMeterPainter(
        peakLDb: peakLDb,
        peakRDb: peakRDb,
        rmsLDb: rmsLDb,
        rmsRDb: rmsRDb,
        grDb: gainReductionDb,
        colors: colors,
        horizontal: horizontal,
      ),
    );
  }
}

class _StereoMeterPainter extends CustomPainter {
  final double peakLDb;
  final double peakRDb;
  final double rmsLDb;
  final double rmsRDb;
  final double grDb;
  final FbColors colors;
  final bool horizontal;

  _StereoMeterPainter({
    required this.peakLDb,
    required this.peakRDb,
    required this.rmsLDb,
    required this.rmsRDb,
    required this.grDb,
    required this.colors,
    required this.horizontal,
  });

  double _dbToNorm(double db) => ((db + 60.0) / 66.0).clamp(0.0, 1.0);

  @override
  void paint(Canvas canvas, Size size) {
    final bgPaint = Paint()..color = colors.bgSunken;
    canvas.drawRect(Offset.zero & size, bgPaint);

    final nl = _dbToNorm(peakLDb);
    final nr = _dbToNorm(peakRDb);

    if (horizontal) {
      final barH = (size.height - 3.0) / 2.0;
      _drawHorizontalBar(canvas, Rect.fromLTWH(1, 1, size.width - 2, barH), nl, peakLDb);
      _drawHorizontalBar(
          canvas, Rect.fromLTWH(1, 2 + barH, size.width - 2, barH), nr, peakRDb);
      return;
    }

    final hasGr = grDb > 0.1;
    final cols = hasGr ? 3 : 2;
    final barW = (size.width - (cols + 1)) / cols;

    _drawVerticalBar(
      canvas,
      Rect.fromLTWH(1, 1, barW, size.height - 2),
      nl,
      peakLDb,
    );
    _drawVerticalBar(
      canvas,
      Rect.fromLTWH(2 + barW, 1, barW, size.height - 2),
      nr,
      peakRDb,
    );
    if (hasGr) {
      final grNorm = (grDb / 24.0).clamp(0.0, 1.0);
      final grH = (size.height - 2) * grNorm;
      canvas.drawRect(
        Rect.fromLTWH(3 + barW * 2, 1, barW, grH),
        Paint()..color = colors.meterOrange,
      );
    }
  }

  void _drawVerticalBar(Canvas canvas, Rect rect, double norm, double db) {
    final fillH = rect.height * norm;
    if (fillH <= 0.5) return;
    final fillRect = Rect.fromLTWH(
      rect.left,
      rect.bottom - fillH,
      rect.width,
      fillH,
    );
    final color = db > -0.5
        ? colors.meterRed
        : (db > -6.0
            ? colors.meterYellow
            : colors.meterGreen);
    canvas.drawRect(fillRect, Paint()..color = color);
  }

  void _drawHorizontalBar(Canvas canvas, Rect rect, double norm, double db) {
    final fillW = rect.width * norm;
    if (fillW <= 0.5) return;
    final fillRect = Rect.fromLTWH(rect.left, rect.top, fillW, rect.height);
    final color = db > -0.5
        ? colors.meterRed
        : (db > -6.0
            ? colors.meterYellow
            : colors.meterGreen);
    canvas.drawRect(fillRect, Paint()..color = color);
  }

  @override
  bool shouldRepaint(covariant _StereoMeterPainter oldDelegate) =>
      oldDelegate.peakLDb != peakLDb ||
      oldDelegate.peakRDb != peakRDb ||
      oldDelegate.grDb != grDb;
}
