import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../l10n/app_localizations.dart';
import '../theme.dart';
import 'components.dart';

/// The edit history of the current file, newest first. Row `i` is the state
/// after step `i` (row 0 is where the history starts); [current] is the
/// state shown now, rows after it are steps that can be redone. Tapping a
/// row jumps there through undo/redo, so nothing is lost either way.
class HistorySheet extends StatelessWidget {
  /// Localised step names; `labels[0]` names the starting state.
  final List<String> labels;
  final int current;
  final ValueChanged<int> onJump;

  const HistorySheet({
    super.key,
    required this.labels,
    required this.current,
    required this.onJump,
  });

  @override
  Widget build(BuildContext context) {
    final l = AppLocalizations.of(context);
    return CantoSheet(
      key: const Key('history-sheet'),
      title: l.history,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        mainAxisSize: MainAxisSize.min,
        children: [
          Text(
            l.historyHint,
            style: CantoTypography.sans(size: 12, color: CantoColors.muted),
          ),
          const SizedBox(height: 10),
          HistoryList(
            labels: labels,
            current: current,
            onJump: onJump,
            scrollable: false,
          ),
        ],
      ),
    );
  }
}

/// The history rows, newest first. Scrollable on its own (side panel) or as
/// a plain column inside a sheet that already scrolls.
class HistoryList extends StatelessWidget {
  final List<String> labels;
  final int current;
  final ValueChanged<int> onJump;
  final bool scrollable;
  final EdgeInsets padding;

  const HistoryList({
    super.key,
    required this.labels,
    required this.current,
    required this.onJump,
    this.scrollable = true,
    this.padding = EdgeInsets.zero,
  });

  @override
  Widget build(BuildContext context) {
    final l = AppLocalizations.of(context);
    if (labels.length <= 1) {
      return Padding(
        padding: padding.add(const EdgeInsets.symmetric(vertical: 18)),
        child: Text(
          l.historyEmpty,
          key: const Key('history-empty'),
          textAlign: TextAlign.center,
          style: CantoTypography.sans(size: 13, color: CantoColors.faint),
        ),
      );
    }
    Widget row(int i) => _HistoryRow(
      key: Key('history-row-$i'),
      label: labels[i],
      step: i,
      state: i == current
          ? _RowState.current
          : i > current
          ? _RowState.undone
          : _RowState.past,
      nowLabel: l.historyNow,
      onTap: i == current ? null : () => onJump(i),
    );
    final last = labels.length - 1;
    if (!scrollable) {
      return Padding(
        padding: padding,
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [for (var i = last; i >= 0; i--) row(i)],
        ),
      );
    }
    return ListView.builder(
      padding: padding,
      itemCount: labels.length,
      itemBuilder: (_, k) => row(last - k),
    );
  }
}

enum _RowState { past, current, undone }

class _HistoryRow extends StatelessWidget {
  final String label;
  final int step;
  final _RowState state;
  final String nowLabel;
  final VoidCallback? onTap;

  const _HistoryRow({
    super.key,
    required this.label,
    required this.step,
    required this.state,
    required this.nowLabel,
    required this.onTap,
  });

  @override
  Widget build(BuildContext context) {
    final current = state == _RowState.current;
    final undone = state == _RowState.undone;
    return Pressable(
      onTap: onTap,
      semanticLabel: label,
      child: Container(
        height: 44,
        margin: const EdgeInsets.only(bottom: 2),
        decoration: BoxDecoration(
          color: current ? CantoColors.selected : Colors.transparent,
          borderRadius: BorderRadius.circular(CantoRadii.chip),
        ),
        child: Row(
          children: [
            // DESIGN.md selection: selected fill plus a leading accent marker.
            Container(
              width: 3,
              height: 20,
              decoration: BoxDecoration(
                color: current ? CantoColors.accent : Colors.transparent,
                borderRadius: BorderRadius.circular(2),
              ),
            ),
            const SizedBox(width: 10),
            SizedBox(
              width: 26,
              child: Text(
                step == 0 ? '' : '$step',
                style: CantoTypography.num(size: 11, color: CantoColors.faint),
              ),
            ),
            Icon(
              step == 0
                  ? LucideIcons.flag
                  : undone
                  ? LucideIcons.redo2
                  : LucideIcons.history,
              size: 15,
              color: undone ? CantoColors.faint : CantoColors.muted,
            ),
            const SizedBox(width: 10),
            Expanded(
              child: Text(
                label,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: CantoTypography.sans(
                  size: 13.5,
                  weight: current ? FontWeight.w600 : FontWeight.w500,
                  color: undone
                      ? CantoColors.faint
                      : current
                      ? CantoColors.inkBright
                      : CantoColors.ink,
                ),
              ),
            ),
            if (current)
              Padding(
                padding: const EdgeInsets.only(right: 10),
                child: Text(
                  nowLabel,
                  style: CantoTypography.sans(
                    size: 11.5,
                    weight: FontWeight.w600,
                    color: CantoColors.muted,
                  ),
                ),
              ),
          ],
        ),
      ),
    );
  }
}
