import 'dart:async';

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../l10n/app_localizations.dart';
import '../music.dart';
import '../theme.dart';
import 'components.dart';

enum AdvancedOp { center, drift, quantize, transpose, level, accentBeats }

/// One macro run from the Advanced tools sheet.
class AdvancedRequest {
  final AdvancedOp op;

  /// Only the selected note, otherwise every note.
  final bool selectedOnly;

  /// 0..1 strength (center, drift, quantize, level).
  final double amount;

  /// Quantize grid in beats (1 = quarter note).
  final double gridBeats;

  /// Transpose interval.
  final int semitones;

  const AdvancedRequest({
    required this.op,
    required this.selectedOnly,
    this.amount = 1,
    this.gridBeats = 0.5,
    this.semitones = 0,
  });
}

/// Melodyne-style macros on one sheet. Each card applies immediately as a
/// single undo step; the sheet stays open so several can be combined, and
/// reports how many notes each run changed.
class AdvancedToolsSheet extends StatefulWidget {
  final bool hasSelection;

  /// Applies the request and returns the number of notes changed.
  final int Function(AdvancedRequest request) onApply;

  /// The editor's undo/redo, so a run can be taken back without leaving the
  /// sheet. The getters are read on every rebuild.
  final ValueGetter<bool> canUndo;
  final ValueGetter<bool> canRedo;
  final VoidCallback onUndo;
  final VoidCallback onRedo;

  const AdvancedToolsSheet({
    super.key,
    required this.hasSelection,
    required this.onApply,
    required this.canUndo,
    required this.canRedo,
    required this.onUndo,
    required this.onRedo,
  });

  @override
  State<AdvancedToolsSheet> createState() => _AdvancedToolsSheetState();
}

class _AdvancedToolsSheetState extends State<AdvancedToolsSheet> {
  late bool _selectedOnly = widget.hasSelection;
  double _center = 100;
  double _drift = 50;
  double _quantize = 100;
  double _grid = 0.5;
  int _semitones = 0;
  double _level = 50;
  double _accent = 60;
  String? _message;
  Timer? _messageTimer;

  @override
  void dispose() {
    _messageTimer?.cancel();
    super.dispose();
  }

  void _run(AdvancedRequest request) {
    final count = widget.onApply(request);
    _messageTimer?.cancel();
    setState(() => _message = AppLocalizations.of(context).advApplied(count));
    _messageTimer = Timer(const Duration(seconds: 3), () {
      if (mounted) setState(() => _message = null);
    });
  }

  @override
  Widget build(BuildContext context) {
    final l = AppLocalizations.of(context);
    return CantoSheet(
      key: const Key('advanced-sheet'),
      title: l.advancedTools,
      trailing: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          CantoIconButton(
            key: const Key('adv-undo'),
            icon: LucideIcons.undo2,
            tooltip: l.undo,
            size: 40,
            onTap: widget.canUndo()
                ? () => setState(() {
                    widget.onUndo();
                    _message = null;
                  })
                : null,
          ),
          CantoIconButton(
            key: const Key('adv-redo'),
            icon: LucideIcons.redo2,
            tooltip: l.redo,
            size: 40,
            onTap: widget.canRedo()
                ? () => setState(() {
                    widget.onRedo();
                    _message = null;
                  })
                : null,
          ),
        ],
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        mainAxisSize: MainAxisSize.min,
        children: [
          Row(
            children: [
              Expanded(
                child: Text(
                  l.advScope,
                  style: CantoTypography.sans(
                    size: 13,
                    weight: FontWeight.w600,
                    color: CantoColors.ink,
                  ),
                ),
              ),
              CantoSegmented<bool>(
                key: const Key('adv-scope'),
                selected: _selectedOnly,
                onChanged: (v) {
                  if (v && !widget.hasSelection) return;
                  setState(() => _selectedOnly = v);
                },
                options: [
                  SegmentOption(
                    false,
                    l.advScopeAll,
                    key: const Key('adv-scope-all'),
                  ),
                  if (widget.hasSelection)
                    SegmentOption(
                      true,
                      l.advScopeSelected,
                      key: const Key('adv-scope-selected'),
                    ),
                ],
              ),
            ],
          ),
          AnimatedSize(
            duration: const Duration(milliseconds: 160),
            child: _message == null
                ? const SizedBox(width: double.infinity)
                : Padding(
                    padding: const EdgeInsets.only(top: 10),
                    child: Row(
                      children: [
                        const Icon(
                          LucideIcons.circleCheck,
                          size: 15,
                          color: CantoColors.success,
                        ),
                        const SizedBox(width: 6),
                        Text(
                          _message!,
                          key: const Key('adv-result'),
                          style: CantoTypography.sans(
                            size: 12.5,
                            weight: FontWeight.w600,
                            color: CantoColors.ink,
                          ),
                        ),
                      ],
                    ),
                  ),
          ),
          _ToolCard(
            icon: LucideIcons.alignCenterHorizontal,
            title: l.advCenter,
            hint: l.advCenterHint,
            applyKey: const Key('adv-center-apply'),
            onApply: () => _run(
              AdvancedRequest(
                op: AdvancedOp.center,
                selectedOnly: _selectedOnly,
                amount: _center / 100,
              ),
            ),
            child: _AmountRow(
              label: l.advAmount,
              value: _center,
              onChanged: (v) => setState(() => _center = v),
            ),
          ),
          _ToolCard(
            icon: LucideIcons.waves,
            title: l.advDrift,
            hint: l.advDriftHint,
            applyKey: const Key('adv-drift-apply'),
            onApply: () => _run(
              AdvancedRequest(
                op: AdvancedOp.drift,
                selectedOnly: _selectedOnly,
                amount: _drift / 100,
              ),
            ),
            child: _AmountRow(
              label: l.advAmount,
              value: _drift,
              onChanged: (v) => setState(() => _drift = v),
            ),
          ),
          _ToolCard(
            icon: LucideIcons.grid3x3,
            title: l.advQuantize,
            hint: l.advQuantizeHint,
            applyKey: const Key('adv-quantize-apply'),
            onApply: () => _run(
              AdvancedRequest(
                op: AdvancedOp.quantize,
                selectedOnly: _selectedOnly,
                amount: _quantize / 100,
                gridBeats: _grid,
              ),
            ),
            child: Column(
              children: [
                Row(
                  children: [
                    SizedBox(
                      width: 78,
                      child: Text(
                        l.advGrid,
                        style: CantoTypography.sans(
                          size: 11.5,
                          color: CantoColors.muted,
                        ),
                      ),
                    ),
                    CantoSegmented<double>(
                      key: const Key('adv-grid'),
                      selected: _grid,
                      onChanged: (v) => setState(() => _grid = v),
                      options: const [
                        SegmentOption(1.0, '1/4'),
                        SegmentOption(0.5, '1/8', key: Key('adv-grid-8')),
                        SegmentOption(0.25, '1/16', key: Key('adv-grid-16')),
                      ],
                    ),
                  ],
                ),
                _AmountRow(
                  label: l.advStrength,
                  value: _quantize,
                  onChanged: (v) => setState(() => _quantize = v),
                ),
              ],
            ),
          ),
          _ToolCard(
            icon: LucideIcons.arrowUpDown,
            title: l.advTranspose,
            hint: l.advTransposeHint,
            applyKey: const Key('adv-transpose-apply'),
            onApply: _semitones == 0
                ? null
                : () => _run(
                    AdvancedRequest(
                      op: AdvancedOp.transpose,
                      selectedOnly: _selectedOnly,
                      semitones: _semitones,
                    ),
                  ),
            child: Row(
              mainAxisAlignment: MainAxisAlignment.center,
              children: [
                CantoIconButton(
                  key: const Key('adv-transpose-down'),
                  icon: LucideIcons.minus,
                  onTap: _semitones > -12
                      ? () => setState(() => _semitones--)
                      : null,
                ),
                SizedBox(
                  width: 96,
                  child: Text(
                    '${formatSigned(_semitones.toDouble(), 0)} st',
                    key: const Key('adv-transpose-value'),
                    textAlign: TextAlign.center,
                    style: CantoTypography.num(
                      size: 20,
                      weight: FontWeight.w600,
                      color: CantoColors.inkBright,
                    ),
                  ),
                ),
                CantoIconButton(
                  key: const Key('adv-transpose-up'),
                  icon: LucideIcons.plus,
                  onTap: _semitones < 12
                      ? () => setState(() => _semitones++)
                      : null,
                ),
              ],
            ),
          ),
          _ToolCard(
            icon: LucideIcons.chartNoAxesColumn,
            title: l.advLevel,
            hint: l.advLevelHint,
            applyKey: const Key('adv-level-apply'),
            onApply: () => _run(
              AdvancedRequest(
                op: AdvancedOp.level,
                selectedOnly: _selectedOnly,
                amount: _level / 100,
              ),
            ),
            child: _AmountRow(
              label: l.advAmount,
              value: _level,
              onChanged: (v) => setState(() => _level = v),
            ),
          ),
          _ToolCard(
            icon: LucideIcons.chevronRight,
            title: l.advAccentBeats,
            hint: l.advAccentBeatsHint,
            applyKey: const Key('adv-accent-apply'),
            onApply: () => _run(
              AdvancedRequest(
                op: AdvancedOp.accentBeats,
                selectedOnly: _selectedOnly,
                amount: _accent / 100,
              ),
            ),
            child: _AmountRow(
              label: l.advStrength,
              value: _accent,
              onChanged: (v) => setState(() => _accent = v),
            ),
          ),
        ],
      ),
    );
  }
}

class _ToolCard extends StatelessWidget {
  final IconData icon;
  final String title;
  final String hint;
  final Key applyKey;
  final VoidCallback? onApply;
  final Widget child;

  const _ToolCard({
    required this.icon,
    required this.title,
    required this.hint,
    required this.applyKey,
    required this.onApply,
    required this.child,
  });

  @override
  Widget build(BuildContext context) {
    final l = AppLocalizations.of(context);
    return Container(
      margin: const EdgeInsets.only(top: 10),
      padding: const EdgeInsets.fromLTRB(14, 12, 10, 10),
      decoration: BoxDecoration(
        color: CantoColors.raised,
        borderRadius: BorderRadius.circular(CantoRadii.card),
        border: Border.all(color: CantoColors.line),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Padding(
                padding: const EdgeInsets.only(top: 1),
                child: Icon(icon, size: 16, color: CantoColors.muted),
              ),
              const SizedBox(width: 8),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(
                      title,
                      style: CantoTypography.sans(
                        size: 13.5,
                        weight: FontWeight.w600,
                        color: CantoColors.inkBright,
                      ),
                    ),
                    const SizedBox(height: 2),
                    Text(
                      hint,
                      style: CantoTypography.sans(
                        size: 11.5,
                        color: CantoColors.muted,
                      ),
                    ),
                  ],
                ),
              ),
              const SizedBox(width: 8),
              CantoButton(
                key: applyKey,
                label: l.apply,
                height: 32,
                style: CantoButtonStyle.tonal,
                onTap: onApply,
              ),
            ],
          ),
          const SizedBox(height: 6),
          child,
        ],
      ),
    );
  }
}

class _AmountRow extends StatelessWidget {
  final String label;
  final double value;
  final ValueChanged<double> onChanged;

  const _AmountRow({
    required this.label,
    required this.value,
    required this.onChanged,
  });

  @override
  Widget build(BuildContext context) {
    return Row(
      children: [
        SizedBox(
          width: 78,
          child: Text(
            label,
            style: CantoTypography.sans(size: 11.5, color: CantoColors.muted),
          ),
        ),
        Expanded(
          child: CantoSlider(
            value: value,
            min: 0,
            max: 100,
            bipolar: false,
            disabled: false,
            onChanged: (v) => onChanged(v.roundToDouble()),
          ),
        ),
        SizedBox(
          width: 44,
          child: Text(
            '${value.round()}%',
            textAlign: TextAlign.right,
            style: CantoTypography.num(
              size: 12,
              weight: FontWeight.w600,
              color: CantoColors.ink,
            ),
          ),
        ),
      ],
    );
  }
}
