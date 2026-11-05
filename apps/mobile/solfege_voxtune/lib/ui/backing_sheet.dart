import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../l10n/app_localizations.dart';
import '../music.dart';
import '../theme.dart';
import 'components.dart';

/// Lowest and highest backing-track volume the sheet offers, in dB.
const double kBackingMinDb = -24;
const double kBackingMaxDb = 6;

/// What the backing-track sheet shows; the editor publishes a new one after
/// every change, so the open sheet follows loading and edits live.
@immutable
class BackingView {
  /// The track's file name; null when the project has no backing track.
  final String? name;
  final bool loading;

  /// Why the track could not be opened, if it could not.
  final String? error;

  /// Length in seconds; 0 until decoded.
  final double duration;
  final double gainDb;
  final bool muted;

  /// Where the edited take starts in the track; null with no take open.
  final double? offset;

  const BackingView({
    this.name,
    this.loading = false,
    this.error,
    this.duration = 0,
    this.gainDb = 0,
    this.muted = false,
    this.offset,
  });

  static const BackingView none = BackingView();
}

/// `m:ss.mmm`, signed when the take starts before the track.
String formatBackingOffset(double sec) {
  final ms = (sec * 1000).round();
  final sign = ms < 0 ? '−' : '';
  final abs = ms.abs();
  final minutes = abs ~/ 60000;
  final seconds = (abs % 60000) ~/ 1000;
  final millis = abs % 1000;
  return '$sign$minutes:${seconds.toString().padLeft(2, '0')}'
      '.${millis.toString().padLeft(3, '0')}';
}

/// The project's backing track: add, replace or remove it, set its volume,
/// and line the edited take up with the song.
class BackingSheet extends StatelessWidget {
  final ValueListenable<BackingView> view;
  final VoidCallback onPick;
  final VoidCallback onRemove;

  /// True plays the track under the vocal, false mutes it.
  final ValueChanged<bool> onPlay;
  final ValueChanged<double> onGainDb;

  /// The edited take's new start in the track, in seconds.
  final ValueChanged<double> onOffset;

  const BackingSheet({
    super.key,
    required this.view,
    required this.onPick,
    required this.onRemove,
    required this.onPlay,
    required this.onGainDb,
    required this.onOffset,
  });

  @override
  Widget build(BuildContext context) {
    final l = AppLocalizations.of(context);
    return ValueListenableBuilder<BackingView>(
      valueListenable: view,
      builder: (context, v, _) => CantoSheet(
        key: const Key('backing-sheet'),
        title: l.backingTrack,
        child: v.name == null ? _buildEmpty(l) : _buildTrack(l, v),
      ),
    );
  }

  Widget _buildEmpty(AppLocalizations l) => SheetAction(
    key: const Key('backing-add'),
    icon: LucideIcons.fileMusic,
    title: l.backingAdd,
    subtitle: l.backingAddHint,
    onTap: onPick,
  );

  Widget _buildTrack(AppLocalizations l, BackingView v) {
    final ready = !v.loading && v.error == null;
    final status = v.loading
        ? l.backingLoading
        : v.error != null
        ? l.backingFailed(v.error!)
        : formatClock(v.duration);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      mainAxisSize: MainAxisSize.min,
      children: [
        Row(
          children: [
            Container(
              width: 40,
              height: 40,
              decoration: BoxDecoration(
                color: CantoColors.raised,
                borderRadius: BorderRadius.circular(CantoRadii.chip + 2),
                border: Border.all(color: CantoColors.line),
              ),
              child: v.loading
                  ? const Padding(
                      padding: EdgeInsets.all(11),
                      child: CircularProgressIndicator(
                        strokeWidth: 2,
                        color: CantoColors.accent,
                      ),
                    )
                  : const Icon(
                      LucideIcons.fileMusic,
                      size: 18,
                      color: CantoColors.accent,
                    ),
            ),
            const SizedBox(width: 12),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(
                    v.name!,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: CantoTypography.sans(
                      size: 14,
                      weight: FontWeight.w600,
                      color: CantoColors.inkBright,
                    ),
                  ),
                  const SizedBox(height: 2),
                  Text(
                    status,
                    maxLines: 2,
                    overflow: TextOverflow.ellipsis,
                    style: CantoTypography.sans(
                      size: 11.5,
                      color: v.error != null
                          ? CantoColors.heat
                          : CantoColors.muted,
                    ),
                  ),
                ],
              ),
            ),
            CantoIconButton(
              key: const Key('backing-play-toggle'),
              icon: v.muted ? LucideIcons.volumeX : LucideIcons.volume2,
              active: !v.muted,
              tooltip: l.backingPlaySemantic,
              color: v.muted ? CantoColors.muted : CantoColors.accent,
              onTap: ready ? () => onPlay(v.muted) : null,
            ),
          ],
        ),
        const SizedBox(height: 12),
        _row(
          label: l.backingVolume,
          child: CantoSlider(
            key: const Key('backing-volume'),
            value: v.gainDb.clamp(kBackingMinDb, kBackingMaxDb),
            min: kBackingMinDb,
            max: kBackingMaxDb,
            bipolar: false,
            disabled: !ready || v.muted,
            onChanged: (db) => onGainDb((db * 2).round() / 2),
          ),
          value: formatDb(v.gainDb),
        ),
        const SizedBox(height: 8),
        Text(
          l.backingOffset.toUpperCase(),
          style: CantoTypography.label(text: l.backingOffset),
        ),
        const SizedBox(height: 6),
        if (v.offset == null)
          Text(
            l.backingNoTake,
            style: CantoTypography.sans(size: 12, color: CantoColors.muted),
          )
        else ...[
          Row(
            children: [
              _nudge('backing-offset-minus-1s', '−1 s', v, -1, ready),
              _nudge('backing-offset-minus-10ms', '−10 ms', v, -0.01, ready),
              Expanded(
                child: Text(
                  formatBackingOffset(v.offset!),
                  key: const Key('backing-offset-value'),
                  textAlign: TextAlign.center,
                  style: CantoTypography.num(
                    size: 15,
                    weight: FontWeight.w600,
                    color: CantoColors.ink,
                  ),
                ),
              ),
              _nudge('backing-offset-plus-10ms', '+10 ms', v, 0.01, ready),
              _nudge('backing-offset-plus-1s', '+1 s', v, 1, ready),
            ],
          ),
          const SizedBox(height: 6),
          Row(
            children: [
              Expanded(
                child: Text(
                  l.backingOffsetHint,
                  style: CantoTypography.sans(
                    size: 11.5,
                    color: CantoColors.muted,
                  ),
                ),
              ),
              CantoButton(
                key: const Key('backing-offset-reset'),
                label: l.backingOffsetReset,
                style: CantoButtonStyle.ghost,
                height: 32,
                onTap: ready && v.offset != 0 ? () => onOffset(0) : null,
              ),
            ],
          ),
        ],
        const SizedBox(height: 8),
        Container(height: 1, color: CantoColors.line),
        const SizedBox(height: 4),
        SheetAction(
          key: const Key('backing-replace'),
          icon: LucideIcons.replace,
          title: l.backingReplace,
          subtitle: l.backingReplaceHint,
          onTap: v.loading ? null : onPick,
        ),
        SheetAction(
          key: const Key('backing-remove'),
          icon: LucideIcons.trash2,
          title: l.backingRemove,
          subtitle: l.backingRemoveHint,
          onTap: onRemove,
        ),
      ],
    );
  }

  Widget _nudge(String key, String label, BackingView v, double by, bool on) {
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 2),
      child: CantoButton(
        key: Key(key),
        label: label,
        style: CantoButtonStyle.outline,
        height: 32,
        onTap: on
            ? () => onOffset(((v.offset! + by) * 1000).round() / 1000)
            : null,
      ),
    );
  }

  Widget _row({
    required String label,
    required Widget child,
    required String value,
  }) {
    return SizedBox(
      height: 40,
      child: Row(
        children: [
          SizedBox(
            width: 64,
            child: FittedBox(
              fit: BoxFit.scaleDown,
              alignment: Alignment.centerLeft,
              child: Text(
                label.toUpperCase(),
                maxLines: 1,
                style: CantoTypography.label(text: label),
              ),
            ),
          ),
          Expanded(child: child),
          SizedBox(
            width: 58,
            child: Text(
              value,
              textAlign: TextAlign.right,
              style: CantoTypography.num(
                size: 12,
                weight: FontWeight.w600,
                color: CantoColors.ink,
              ),
            ),
          ),
        ],
      ),
    );
  }
}
