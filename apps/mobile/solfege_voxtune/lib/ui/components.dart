import 'dart:math' as math;

import 'package:flutter/material.dart';

import '../music.dart';
import '../theme.dart';

/// Tap target with a subtle pressed state. Every touchable in the app goes
/// through this so feedback is consistent.
class Pressable extends StatefulWidget {
  final VoidCallback? onTap;
  final VoidCallback? onLongPress;
  final Widget child;
  final String? semanticLabel;

  const Pressable({
    super.key,
    required this.onTap,
    required this.child,
    this.onLongPress,
    this.semanticLabel,
  });

  @override
  State<Pressable> createState() => _PressableState();
}

class _PressableState extends State<Pressable> {
  bool _down = false;

  void _set(bool down) {
    if (_down != down) setState(() => _down = down);
  }

  @override
  Widget build(BuildContext context) {
    final enabled = widget.onTap != null;
    return Semantics(
      button: true,
      enabled: enabled,
      label: widget.semanticLabel,
      child: GestureDetector(
        behavior: HitTestBehavior.opaque,
        onTap: widget.onTap,
        onLongPress: widget.onLongPress,
        onTapDown: enabled ? (_) => _set(true) : null,
        onTapUp: enabled ? (_) => _set(false) : null,
        onTapCancel: enabled ? () => _set(false) : null,
        child: AnimatedOpacity(
          duration: const Duration(milliseconds: 90),
          opacity: _down ? 0.62 : 1.0,
          child: widget.child,
        ),
      ),
    );
  }
}

class CantoIconButton extends StatelessWidget {
  final IconData icon;
  final VoidCallback? onTap;
  final VoidCallback? onLongPress;
  final bool active;
  final double size;
  final double iconSize;
  final String? tooltip;
  final Color? color;

  const CantoIconButton({
    super.key,
    required this.icon,
    required this.onTap,
    this.onLongPress,
    this.active = false,
    this.size = 44,
    this.iconSize = 19,
    this.tooltip,
    this.color,
  });

  @override
  Widget build(BuildContext context) {
    final enabled = onTap != null;
    final fg = !enabled
        ? CantoColors.faint.withValues(alpha: 0.55)
        : active
        ? CantoColors.ink
        : (color ?? CantoColors.ink);
    return Pressable(
      onTap: onTap,
      onLongPress: onLongPress,
      semanticLabel: tooltip,
      child: SizedBox(
        width: size,
        height: size,
        child: Center(
          child: AnimatedContainer(
            duration: const Duration(milliseconds: 140),
            width: size - 6,
            height: size - 6,
            // Latched state (e.g. loop): selected fill + border, never the
            // accent (DESIGN.md latched-toggle rule).
            decoration: BoxDecoration(
              color: active ? CantoColors.selected : Colors.transparent,
              borderRadius: BorderRadius.circular(CantoRadii.chip + 2),
              border: Border.all(
                color: active ? CantoColors.lineStrong : Colors.transparent,
              ),
            ),
            child: Icon(icon, size: iconSize, color: fg),
          ),
        ),
      ),
    );
  }
}

/// [record] uses the theme's arm red (state.arm), reserved for recording.
enum CantoButtonStyle { primary, record, tonal, outline, ghost }

class CantoButton extends StatelessWidget {
  final IconData? icon;
  final String label;
  final VoidCallback? onTap;
  final CantoButtonStyle style;
  final double height;
  final bool expand;

  const CantoButton({
    super.key,
    this.icon,
    required this.label,
    required this.onTap,
    this.style = CantoButtonStyle.tonal,
    this.height = 36,
    this.expand = false,
  });

  @override
  Widget build(BuildContext context) {
    final enabled = onTap != null;
    late final Color bg;
    late final Color fg;
    Color border = Colors.transparent;
    switch (style) {
      case CantoButtonStyle.primary:
        bg = CantoColors.accent;
        fg = CantoColors.accentInk;
      case CantoButtonStyle.record:
        bg = CantoColors.heat;
        fg = const Color(0xFFFFFFFF);
      case CantoButtonStyle.tonal:
        bg = CantoColors.accentSoft;
        fg = CantoColors.accentHover;
        border = CantoColors.accentLine;
      case CantoButtonStyle.outline:
        bg = CantoColors.raised;
        fg = CantoColors.ink;
        border = CantoColors.lineStrong;
      case CantoButtonStyle.ghost:
        bg = Colors.transparent;
        fg = CantoColors.muted;
    }
    final row = Row(
      mainAxisSize: expand ? MainAxisSize.max : MainAxisSize.min,
      mainAxisAlignment: MainAxisAlignment.center,
      children: [
        if (icon != null) ...[
          Icon(icon, size: 15, color: fg),
          const SizedBox(width: 6),
        ],
        Flexible(
          child: Text(
            label,
            overflow: TextOverflow.ellipsis,
            style: CantoTypography.sans(
              size: 12.5,
              weight: FontWeight.w600,
              color: fg,
              letterSpacing: 0.2,
            ),
          ),
        ),
      ],
    );
    return Pressable(
      onTap: onTap,
      child: Opacity(
        opacity: enabled ? 1.0 : 0.38,
        child: Container(
          height: height,
          padding: const EdgeInsets.symmetric(horizontal: 14),
          decoration: BoxDecoration(
            color: bg,
            borderRadius: BorderRadius.circular(height / 2),
            border: Border.all(color: border),
          ),
          child: row,
        ),
      ),
    );
  }
}

/// Compact pill showing a selector value, e.g. the key/scale.
class CantoSelectChip extends StatelessWidget {
  final IconData icon;
  final String label;
  final VoidCallback? onTap;
  final bool open;

  const CantoSelectChip({
    super.key,
    required this.icon,
    required this.label,
    required this.onTap,
    this.open = false,
  });

  @override
  Widget build(BuildContext context) {
    return Pressable(
      onTap: onTap,
      child: Container(
        height: 32,
        padding: const EdgeInsets.only(left: 10, right: 8),
        decoration: BoxDecoration(
          color: CantoColors.raised,
          borderRadius: BorderRadius.circular(16),
          border: Border.all(
            color: open ? CantoColors.accent : CantoColors.lineStrong,
          ),
        ),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(icon, size: 14, color: CantoColors.accent),
            const SizedBox(width: 6),
            Text(
              label,
              style: CantoTypography.sans(
                size: 12.5,
                weight: FontWeight.w600,
                color: CantoColors.inkBright,
              ),
            ),
            const SizedBox(width: 2),
            const Icon(
              Icons.expand_more_rounded,
              size: 16,
              color: CantoColors.muted,
            ),
          ],
        ),
      ),
    );
  }
}

class SegmentOption<T> {
  final T value;
  final String label;
  final Key? key;

  const SegmentOption(this.value, this.label, {this.key});
}

class CantoSegmented<T> extends StatelessWidget {
  final List<SegmentOption<T>> options;
  final T selected;
  final ValueChanged<T> onChanged;
  final double height;

  const CantoSegmented({
    super.key,
    required this.options,
    required this.selected,
    required this.onChanged,
    this.height = 32,
  });

  @override
  Widget build(BuildContext context) {
    return Container(
      height: height,
      padding: const EdgeInsets.all(3),
      decoration: BoxDecoration(
        color: CantoColors.well,
        borderRadius: BorderRadius.circular(height / 2),
        border: Border.all(color: CantoColors.line),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          for (final opt in options)
            Pressable(
              key: opt.key,
              onTap: () => onChanged(opt.value),
              child: AnimatedContainer(
                duration: const Duration(milliseconds: 140),
                padding: const EdgeInsets.symmetric(horizontal: 10),
                alignment: Alignment.center,
                decoration: BoxDecoration(
                  color: opt.value == selected
                      ? CantoColors.high
                      : Colors.transparent,
                  borderRadius: BorderRadius.circular(height / 2),
                ),
                child: Text(
                  opt.label,
                  style: CantoTypography.sans(
                    size: 11.5,
                    weight: FontWeight.w600,
                    color: opt.value == selected
                        ? CantoColors.inkBright
                        : CantoColors.muted,
                  ),
                ),
              ),
            ),
        ],
      ),
    );
  }
}

/// One edit tool: selects how drags on the canvas behave and which parameter
/// the inspector slider edits. Shows the selected note's current value.
class ToolChip extends StatelessWidget {
  final IconData icon;
  final String label;
  final String value;
  final bool active;
  final bool valueMuted;
  final double height;
  final String? semanticLabel;
  final VoidCallback onTap;

  const ToolChip({
    super.key,
    required this.icon,
    required this.label,
    required this.value,
    required this.active,
    required this.onTap,
    this.valueMuted = false,
    this.height = 54,
    this.semanticLabel,
  });

  @override
  Widget build(BuildContext context) {
    return Pressable(
      onTap: onTap,
      semanticLabel: semanticLabel ?? label,
      child: AnimatedContainer(
        duration: const Duration(milliseconds: 140),
        height: height,
        padding: const EdgeInsets.fromLTRB(8, 7, 6, 6),
        decoration: BoxDecoration(
          color: active ? CantoColors.accentSoft : CantoColors.raised,
          borderRadius: BorderRadius.circular(CantoRadii.chip + 2),
          border: Border.all(
            color: active ? CantoColors.accentLine : CantoColors.line,
          ),
        ),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          mainAxisAlignment: MainAxisAlignment.spaceBetween,
          children: [
            FittedBox(
              fit: BoxFit.scaleDown,
              alignment: Alignment.centerLeft,
              child: Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  Icon(
                    icon,
                    size: 13,
                    color: active ? CantoColors.accent : CantoColors.muted,
                  ),
                  const SizedBox(width: 4),
                  Text(
                    label,
                    style: CantoTypography.sans(
                      size: 10.5,
                      weight: FontWeight.w600,
                      color: active ? CantoColors.inkBright : CantoColors.muted,
                    ),
                  ),
                ],
              ),
            ),
            FittedBox(
              fit: BoxFit.scaleDown,
              alignment: Alignment.centerLeft,
              child: Text(
                value,
                style: CantoTypography.num(
                  size: 12,
                  weight: FontWeight.w600,
                  color: valueMuted
                      ? CantoColors.faint
                      : active
                      ? CantoColors.accent
                      : CantoColors.ink,
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class CantoSlider extends StatelessWidget {
  final double value;
  final double min;
  final double max;
  final bool bipolar;
  final bool disabled;
  final VoidCallback? onChangeStart;
  final ValueChanged<double>? onChanged;
  final VoidCallback? onChangeEnd;

  const CantoSlider({
    super.key,
    required this.value,
    required this.min,
    required this.max,
    required this.bipolar,
    required this.disabled,
    this.onChangeStart,
    this.onChanged,
    this.onChangeEnd,
  });

  static const double _inset = 10.0;

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        final width = math.max(1.0, constraints.maxWidth - _inset * 2);
        void handleDx(double dx) {
          if (disabled || onChanged == null) return;
          final t = clampVal((dx - _inset) / width, 0.0, 1.0);
          onChanged!(min + (max - min) * t);
        }

        return Opacity(
          opacity: disabled ? 0.35 : 1.0,
          child: GestureDetector(
            behavior: HitTestBehavior.opaque,
            onHorizontalDragStart: disabled
                ? null
                : (details) {
                    onChangeStart?.call();
                    handleDx(details.localPosition.dx);
                  },
            onHorizontalDragUpdate: disabled
                ? null
                : (details) => handleDx(details.localPosition.dx),
            onHorizontalDragEnd: disabled ? null : (_) => onChangeEnd?.call(),
            onTapDown: disabled
                ? null
                : (details) {
                    onChangeStart?.call();
                    handleDx(details.localPosition.dx);
                    onChangeEnd?.call();
                  },
            child: SizedBox(
              height: 36,
              child: CustomPaint(
                painter: _SliderPainter(
                  value: value,
                  min: min,
                  max: max,
                  bipolar: bipolar,
                  inset: _inset,
                ),
              ),
            ),
          ),
        );
      },
    );
  }
}

class _SliderPainter extends CustomPainter {
  final double value;
  final double min;
  final double max;
  final bool bipolar;
  final double inset;

  const _SliderPainter({
    required this.value,
    required this.min,
    required this.max,
    required this.bipolar,
    required this.inset,
  });

  @override
  void paint(Canvas canvas, Size size) {
    final x0 = inset;
    final w = math.max(1.0, size.width - inset * 2);
    final cy = size.height / 2.0;
    final span = (max - min).abs() < 1e-6 ? 1.0 : (max - min);
    final p = clampVal((value - min) / span, 0.0, 1.0);

    canvas.drawRRect(
      RRect.fromRectAndRadius(
        Rect.fromLTWH(x0, cy - 2, w, 4),
        const Radius.circular(2),
      ),
      Paint()..color = CantoColors.high,
    );

    final active = Paint()..color = CantoColors.accent;
    double a;
    double b;
    if (bipolar) {
      final mid = clampVal((0.0 - min) / span, 0.0, 1.0);
      a = x0 + math.min(mid, p) * w;
      b = x0 + math.max(mid, p) * w;
      final cx = x0 + mid * w;
      canvas.drawRect(
        Rect.fromLTWH(cx - 0.5, cy - 6, 1, 12),
        Paint()..color = CantoColors.lineStrong,
      );
    } else {
      a = x0;
      b = x0 + p * w;
    }
    if (b > a) {
      canvas.drawRRect(
        RRect.fromRectAndRadius(
          Rect.fromLTRB(a, cy - 2, b, cy + 2),
          const Radius.circular(2),
        ),
        active,
      );
    }

    final thumb = Offset(x0 + p * w, cy);
    canvas.drawCircle(
      thumb.translate(0, 1),
      9.5,
      Paint()..color = const Color.fromRGBO(0, 0, 0, 0.45),
    );
    canvas.drawCircle(thumb, 9, Paint()..color = CantoColors.inkBright);
    canvas.drawCircle(thumb, 3.2, Paint()..color = CantoColors.accent);
  }

  @override
  bool shouldRepaint(covariant _SliderPainter old) =>
      old.value != value ||
      old.min != min ||
      old.max != max ||
      old.bipolar != bipolar;
}

/// Tuner-style needle showing how far a note sits from its scale target.
class CentsGauge extends StatelessWidget {
  final int? cents;

  const CentsGauge({super.key, required this.cents});

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      height: 26,
      child: CustomPaint(painter: _CentsGaugePainter(cents)),
    );
  }
}

class _CentsGaugePainter extends CustomPainter {
  final int? cents;

  const _CentsGaugePainter(this.cents);

  @override
  void paint(Canvas canvas, Size size) {
    final w = size.width;
    final cy = size.height / 2.0;
    const range = 50.0;
    double xOf(double c) =>
        (clampVal(c, -range, range) + range) / (range * 2) * w;

    // Acceptable zone (±15¢), the same threshold as the off-key count.
    canvas.drawRRect(
      RRect.fromRectAndRadius(
        Rect.fromLTRB(xOf(-15), cy - 5, xOf(15), cy + 5),
        const Radius.circular(3),
      ),
      Paint()..color = CantoColors.accentSoft,
    );
    for (int c = -50; c <= 50; c += 10) {
      final x = xOf(c.toDouble()).roundToDouble() + 0.5;
      final major = c == 0 || c.abs() == 50;
      final h = c == 0
          ? 7.0
          : major
          ? 5.0
          : 3.0;
      canvas.drawLine(
        Offset(x, cy - h),
        Offset(x, cy + h),
        Paint()
          ..strokeWidth = 1
          ..color = c == 0 ? CantoColors.muted : CantoColors.lineStrong,
      );
    }

    final value = cents;
    if (value == null) return;
    final off = value.abs() >= 15;
    final color = off ? CantoColors.heat : CantoColors.accent;
    final x = xOf(value.toDouble());
    final zero = xOf(0);
    if ((x - zero).abs() > 1) {
      canvas.drawLine(
        Offset(zero, cy),
        Offset(x, cy),
        Paint()
          ..strokeWidth = 2
          ..strokeCap = StrokeCap.round
          ..color = color.withValues(alpha: 0.55),
      );
    }
    final needle = RRect.fromRectAndRadius(
      Rect.fromCenter(center: Offset(x, cy), width: 4, height: size.height - 4),
      const Radius.circular(2),
    );
    canvas.drawRRect(needle, Paint()..color = color);
    if (value.abs() > range) {
      // Past the gauge: draw a chevron at the clamped end.
      final dir = value > 0 ? 1.0 : -1.0;
      final path = Path()
        ..moveTo(x + dir * 4, cy - 4)
        ..lineTo(x + dir * 8, cy)
        ..lineTo(x + dir * 4, cy + 4);
      canvas.drawPath(
        path,
        Paint()
          ..style = PaintingStyle.stroke
          ..strokeWidth = 1.5
          ..color = color,
      );
    }
  }

  @override
  bool shouldRepaint(covariant _CentsGaugePainter old) => old.cents != cents;
}

class LevelMeter extends StatelessWidget {
  final double level;

  const LevelMeter({super.key, required this.level});

  @override
  Widget build(BuildContext context) {
    const shape = [0.35, 0.5, 0.68, 0.84, 1.0];
    return SizedBox(
      height: 20,
      child: Row(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.end,
        children: [
          for (int i = 0; i < 5; i++) ...[
            if (i > 0) const SizedBox(width: 2),
            Builder(
              builder: (context) {
                final lit = level > 0.05 && level >= ((i + 0.45) / 5.0) * 0.9;
                Color color = CantoColors.high;
                if (lit) {
                  color = i == 4
                      ? CantoColors.meterHigh
                      : i == 3
                      ? CantoColors.meterMid
                      : CantoColors.meterLow;
                }
                return Container(
                  width: 3,
                  height: 5 + shape[i] * 15,
                  decoration: BoxDecoration(
                    color: color,
                    borderRadius: BorderRadius.circular(1.5),
                  ),
                );
              },
            ),
          ],
        ],
      ),
    );
  }
}

class CantoLogo extends StatelessWidget {
  final double size;

  const CantoLogo({super.key, this.size = 32});

  @override
  Widget build(BuildContext context) {
    return Container(
      width: size,
      height: size,
      decoration: BoxDecoration(
        color: CantoColors.raised,
        borderRadius: BorderRadius.circular(size * 0.3),
        border: Border.all(color: CantoColors.line),
      ),
      padding: EdgeInsets.all(size * 0.2),
      child: const CustomPaint(painter: _LogoPainter()),
    );
  }
}

class _LogoPainter extends CustomPainter {
  const _LogoPainter();

  @override
  void paint(Canvas canvas, Size size) {
    canvas.save();
    canvas.scale(size.width / 16.0, size.height / 16.0);
    final path = Path()
      ..moveTo(1.4, 10.1)
      ..relativeCubicTo(1.1, -2.5, 2.2, -3.6, 3.5, -2.1)
      ..relativeCubicTo(1.1, 1.3, 1.6, 2.5, 2.9, 2.3)
      ..relativeCubicTo(1.4, -0.2, 1.8, -2.3, 3.1, -2.1)
      ..relativeCubicTo(1.2, 0.2, 1.8, 1.7, 2.9, 0.9);
    canvas.drawPath(
      path,
      Paint()
        ..color = CantoColors.accent
        ..style = PaintingStyle.stroke
        ..strokeWidth = 1.9
        ..strokeCap = StrokeCap.round,
    );
    canvas.restore();
  }

  @override
  bool shouldRepaint(covariant CustomPainter oldDelegate) => false;
}

/// Bottom-sheet scaffold shared by the key and session sheets.
class CantoSheet extends StatelessWidget {
  final String title;
  final Widget child;
  final Widget? trailing;

  const CantoSheet({
    super.key,
    required this.title,
    required this.child,
    this.trailing,
  });

  @override
  Widget build(BuildContext context) {
    return SafeArea(
      top: false,
      child: Padding(
        padding: const EdgeInsets.fromLTRB(16, 8, 16, 16),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Center(
              child: Container(
                width: 36,
                height: 4,
                decoration: BoxDecoration(
                  color: CantoColors.lineStrong,
                  borderRadius: BorderRadius.circular(2),
                ),
              ),
            ),
            const SizedBox(height: 14),
            Row(
              children: [
                Expanded(
                  child: Text(
                    title,
                    style: CantoTypography.sans(
                      size: 17,
                      weight: FontWeight.w600,
                      color: CantoColors.inkBright,
                    ),
                  ),
                ),
                ?trailing,
              ],
            ),
            const SizedBox(height: 14),
            Flexible(child: SingleChildScrollView(child: child)),
          ],
        ),
      ),
    );
  }
}

class SheetAction extends StatelessWidget {
  final IconData icon;
  final String title;
  final String subtitle;
  final VoidCallback? onTap;

  const SheetAction({
    super.key,
    required this.icon,
    required this.title,
    required this.subtitle,
    required this.onTap,
  });

  @override
  Widget build(BuildContext context) {
    final enabled = onTap != null;
    return Pressable(
      onTap: onTap,
      child: Opacity(
        opacity: enabled ? 1.0 : 0.4,
        child: Padding(
          padding: const EdgeInsets.symmetric(vertical: 8),
          child: Row(
            children: [
              Container(
                width: 40,
                height: 40,
                decoration: BoxDecoration(
                  color: CantoColors.raised,
                  borderRadius: BorderRadius.circular(CantoRadii.chip + 2),
                  border: Border.all(color: CantoColors.line),
                ),
                child: Icon(icon, size: 18, color: CantoColors.accent),
              ),
              const SizedBox(width: 12),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(
                      title,
                      style: CantoTypography.sans(
                        size: 14,
                        weight: FontWeight.w600,
                        color: CantoColors.inkBright,
                      ),
                    ),
                    const SizedBox(height: 2),
                    Text(
                      subtitle,
                      style: CantoTypography.sans(
                        size: 11.5,
                        color: CantoColors.muted,
                      ),
                    ),
                  ],
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

/// Floating toolbar over the canvas (note actions). One of the few places
/// with a shadow: it floats above content (DESIGN.md: menus/popovers).
class FloatingActionBar extends StatelessWidget {
  final List<Widget> children;

  const FloatingActionBar({super.key, required this.children});

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.all(4),
      decoration: BoxDecoration(
        color: CantoColors.raised,
        borderRadius: BorderRadius.circular(16),
        border: Border.all(color: CantoColors.lineStrong),
        boxShadow: const [
          BoxShadow(
            color: Color(0x66000000),
            blurRadius: 16,
            offset: Offset(0, 6),
          ),
        ],
      ),
      // Children share the width when it runs short (narrow phones, the
      // landscape canvas) instead of overflowing.
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          for (final c in children) c is Flexible ? c : Flexible(child: c),
        ],
      ),
    );
  }
}

/// Icon-over-label button for [FloatingActionBar]. [active] marks a latched
/// state with fill + border, not the accent; [warning] tints it as a state
/// that removes sound (muted).
class ActionBarButton extends StatelessWidget {
  final IconData icon;
  final String label;
  final VoidCallback? onTap;
  final bool active;
  final bool warning;
  final String? semanticLabel;

  const ActionBarButton({
    super.key,
    required this.icon,
    required this.label,
    required this.onTap,
    this.active = false,
    this.warning = false,
    this.semanticLabel,
  });

  @override
  Widget build(BuildContext context) {
    final enabled = onTap != null;
    final warn = active && warning;
    final fg = warn ? CantoColors.bypass : CantoColors.ink;
    return Pressable(
      onTap: onTap,
      semanticLabel: semanticLabel ?? label,
      child: Opacity(
        opacity: enabled ? 1 : 0.35,
        child: Container(
          constraints: const BoxConstraints(minWidth: 58),
          padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 6),
          decoration: BoxDecoration(
            color: warn
                ? CantoColors.bypassSoft
                : active
                ? CantoColors.selected
                : Colors.transparent,
            borderRadius: BorderRadius.circular(12),
            border: Border.all(
              color: warn
                  ? CantoColors.bypassLine
                  : active
                  ? CantoColors.lineStrong
                  : Colors.transparent,
            ),
          ),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Icon(icon, size: 18, color: fg),
              const SizedBox(height: 3),
              Text(
                label,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: CantoTypography.sans(
                  size: 11,
                  weight: FontWeight.w600,
                  color: fg,
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}
