import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../l10n/app_localizations.dart';
import '../music.dart';
import '../theme.dart';
import 'components.dart';

/// Localised name of a ลูกคอ style.
String ornamentName(AppLocalizations l, int style) => switch (style) {
  kOrnTurnUp => l.ornTurnUp,
  kOrnTurnDown => l.ornTurnDown,
  kOrnShake => l.ornShake,
  kOrnFall => l.ornFall,
  _ => l.ornNone,
};

/// ลูกคอ and accent for one note. Every change goes straight to the engine
/// (so a preview plays it) and each gesture is one undo step, through the
/// editor's callbacks.
class ExpressionSheet extends StatefulWidget {
  /// The note as it is now (null once it no longer exists).
  final VocalNote? Function() note;

  /// Opens an undo step named [label] (`orn` / `accent`).
  final void Function(String label) onBegin;

  /// Applies [change] to the note and syncs it to the engine.
  final void Function(void Function(VocalNote n) change) onUpdate;

  /// Closes the undo step.
  final VoidCallback onEnd;

  /// Plays the note once with its current settings.
  final VoidCallback onPreview;

  const ExpressionSheet({
    super.key,
    required this.note,
    required this.onBegin,
    required this.onUpdate,
    required this.onEnd,
    required this.onPreview,
  });

  @override
  State<ExpressionSheet> createState() => _ExpressionSheetState();
}

class _ExpressionSheetState extends State<ExpressionSheet> {
  /// A one-tap edit (style, clear) as its own undo step.
  void _step(String label, void Function(VocalNote n) change) {
    widget.onBegin(label);
    widget.onUpdate(change);
    widget.onEnd();
    setState(() {});
  }

  @override
  Widget build(BuildContext context) {
    final l = AppLocalizations.of(context);
    final note = widget.note();
    if (note == null) return const SizedBox.shrink();
    final hasOrn = note.ornStyle != kOrnNone;
    return CantoSheet(
      key: const Key('expression-sheet'),
      title: l.expressionTitle,
      trailing: CantoButton(
        key: const Key('expr-preview'),
        label: l.listen,
        icon: LucideIcons.play,
        height: 34,
        style: CantoButtonStyle.tonal,
        onTap: widget.onPreview,
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        mainAxisSize: MainAxisSize.min,
        children: [
          _SectionLabel(icon: LucideIcons.spline, text: l.ornament),
          const SizedBox(height: 8),
          Row(
            children: [
              for (final (i, style) in const [
                kOrnNone,
                kOrnTurnUp,
                kOrnTurnDown,
                kOrnShake,
                kOrnFall,
              ].indexed) ...[
                if (i > 0) const SizedBox(width: 6),
                Expanded(
                  child: _StyleTile(
                    key: Key('orn-$style'),
                    style: style,
                    label: ornamentName(l, style),
                    selected: note.ornStyle == style,
                    onTap: note.ornStyle == style
                        ? null
                        : () => _step('orn', (n) => n.ornStyle = style),
                  ),
                ),
              ],
            ],
          ),
          const SizedBox(height: 6),
          _SliderRow(
            key: const Key('orn-depth'),
            label: l.ornDepth,
            value: note.ornDepth,
            min: 0.25,
            max: 3,
            text: '${note.ornDepth.toStringAsFixed(2)} st',
            enabled: hasOrn,
            onStart: () => widget.onBegin('orn'),
            onChanged: (v) {
              widget.onUpdate((n) => n.ornDepth = (v * 20).round() / 20);
              setState(() {});
            },
            onEnd: widget.onEnd,
          ),
          _SliderRow(
            key: const Key('orn-speed'),
            label: l.ornSpeed,
            value: note.ornSpeed * 100,
            min: 0,
            max: 100,
            text: '${(note.ornSpeed * 100).round()}%',
            enabled: hasOrn,
            onStart: () => widget.onBegin('orn'),
            onChanged: (v) {
              widget.onUpdate((n) => n.ornSpeed = v.roundToDouble() / 100);
              setState(() {});
            },
            onEnd: widget.onEnd,
          ),
          const SizedBox(height: 10),
          Container(height: 1, color: CantoColors.line),
          const SizedBox(height: 12),
          _SectionLabel(icon: LucideIcons.chevronRight, text: l.accent),
          const SizedBox(height: 2),
          Text(
            l.accentHint,
            style: CantoTypography.sans(size: 11.5, color: CantoColors.muted),
          ),
          _SliderRow(
            key: const Key('accent-amount'),
            label: l.accentAmount,
            value: note.accent * 100,
            min: 0,
            max: 100,
            text: '+${(note.accent * 6).toStringAsFixed(1)} dB',
            enabled: true,
            onStart: () => widget.onBegin('accent'),
            onChanged: (v) {
              widget.onUpdate((n) => n.accent = v.roundToDouble() / 100);
              setState(() {});
            },
            onEnd: widget.onEnd,
          ),
          const SizedBox(height: 8),
          Align(
            alignment: Alignment.centerRight,
            child: CantoButton(
              key: const Key('expr-clear'),
              label: l.clearExpression,
              icon: LucideIcons.eraser,
              height: 34,
              style: CantoButtonStyle.ghost,
              onTap: note.hasExpression
                  ? () => _step('exprClear', (n) {
                      n
                        ..ornStyle = kOrnNone
                        ..accent = 0;
                    })
                  : null,
            ),
          ),
        ],
      ),
    );
  }
}

class _SectionLabel extends StatelessWidget {
  final IconData icon;
  final String text;

  const _SectionLabel({required this.icon, required this.text});

  @override
  Widget build(BuildContext context) {
    return Row(
      children: [
        Icon(icon, size: 15, color: CantoColors.muted),
        const SizedBox(width: 7),
        Text(
          text,
          style: CantoTypography.sans(
            size: 13.5,
            weight: FontWeight.w600,
            color: CantoColors.inkBright,
          ),
        ),
      ],
    );
  }
}

/// A style choice drawn as its real pitch shape. Selected: selected fill
/// plus a leading accent marker (DESIGN.md).
class _StyleTile extends StatelessWidget {
  final int style;
  final String label;
  final bool selected;
  final VoidCallback? onTap;

  const _StyleTile({
    super.key,
    required this.style,
    required this.label,
    required this.selected,
    required this.onTap,
  });

  @override
  Widget build(BuildContext context) {
    return Pressable(
      onTap: onTap,
      semanticLabel: label,
      child: Container(
        height: 70,
        decoration: BoxDecoration(
          color: selected ? CantoColors.selected : CantoColors.raised,
          borderRadius: BorderRadius.circular(CantoRadii.chip + 2),
          border: Border.all(
            color: selected ? CantoColors.lineStrong : CantoColors.line,
          ),
        ),
        child: Stack(
          children: [
            Positioned(
              left: 0,
              top: 22,
              bottom: 22,
              child: Container(
                width: 3,
                decoration: BoxDecoration(
                  color: selected ? CantoColors.accent : Colors.transparent,
                  borderRadius: BorderRadius.circular(2),
                ),
              ),
            ),
            Padding(
              padding: const EdgeInsets.fromLTRB(9, 8, 6, 6),
              child: Column(
                children: [
                  Expanded(
                    child: CustomPaint(
                      size: Size.infinite,
                      painter: _ShapePainter(style: style, selected: selected),
                    ),
                  ),
                  const SizedBox(height: 4),
                  Text(
                    label,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: CantoTypography.sans(
                      size: 11,
                      weight: FontWeight.w600,
                      color: selected ? CantoColors.inkBright : CantoColors.ink,
                    ),
                  ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _ShapePainter extends CustomPainter {
  final int style;
  final bool selected;

  _ShapePainter({required this.style, required this.selected});

  @override
  void paint(Canvas canvas, Size size) {
    final demo = VocalNote(
      id: 'demo',
      lyric: '',
      unpitched: false,
      detected: 60,
      pitch: 60,
      detectedStart: 0,
      start: 0,
      detectedDur: 0.8,
      dur: 0.8,
      gainDb: 0,
      detectedGain: 0,
      ornStyle: style,
      ornDepth: 1,
      ornSpeed: 0.5,
    );
    final mid = size.height / 2;
    final scale = size.height * 0.36;
    final path = Path();
    const steps = 60;
    for (var k = 0; k <= steps; k++) {
      final t = k / steps;
      final x = t * size.width;
      final y = mid - ornamentSemis(demo, t) * scale;
      k == 0 ? path.moveTo(x, y) : path.lineTo(x, y);
    }
    canvas.drawPath(
      path,
      Paint()
        ..style = PaintingStyle.stroke
        ..strokeWidth = 1.8
        ..strokeCap = StrokeCap.round
        ..strokeJoin = StrokeJoin.round
        ..color = selected ? CantoColors.accent : CantoColors.canvasMuted,
    );
  }

  @override
  bool shouldRepaint(covariant _ShapePainter old) =>
      old.style != style || old.selected != selected;
}

class _SliderRow extends StatelessWidget {
  final String label;
  final double value;
  final double min;
  final double max;
  final String text;
  final bool enabled;
  final VoidCallback onStart;
  final ValueChanged<double> onChanged;
  final VoidCallback onEnd;

  const _SliderRow({
    super.key,
    required this.label,
    required this.value,
    required this.min,
    required this.max,
    required this.text,
    required this.enabled,
    required this.onStart,
    required this.onChanged,
    required this.onEnd,
  });

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      height: 40,
      child: Row(
        children: [
          SizedBox(
            width: 78,
            child: Text(
              label,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: CantoTypography.sans(size: 11.5, color: CantoColors.muted),
            ),
          ),
          Expanded(
            child: CantoSlider(
              value: math.min(max, math.max(min, value)),
              min: min,
              max: max,
              bipolar: false,
              disabled: !enabled,
              onChangeStart: onStart,
              onChanged: onChanged,
              onChangeEnd: onEnd,
            ),
          ),
          SizedBox(
            width: 64,
            child: Text(
              text,
              textAlign: TextAlign.right,
              style: CantoTypography.num(
                size: 12,
                weight: FontWeight.w600,
                color: enabled ? CantoColors.ink : CantoColors.faint,
              ),
            ),
          ),
        ],
      ),
    );
  }
}
