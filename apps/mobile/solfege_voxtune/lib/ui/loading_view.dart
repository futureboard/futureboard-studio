import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../l10n/app_localizations.dart';
import '../rust_dsp.dart';
import '../theme.dart';
import 'components.dart';

/// Progress of one file load, as reported by the native worker.
class LoadingCard extends StatelessWidget {
  final String fileName;
  final LoadStage stage;

  /// Analysis progress 0..1.
  final double progress;

  /// A cached analysis exists, so no analysis is expected.
  final bool hasCache;
  final bool reanalyze;
  final bool cancelling;
  final VoidCallback? onCancel;

  const LoadingCard({
    super.key,
    required this.fileName,
    required this.stage,
    required this.progress,
    required this.hasCache,
    required this.reanalyze,
    required this.cancelling,
    required this.onCancel,
  });

  @override
  Widget build(BuildContext context) {
    final l = AppLocalizations.of(context);
    // Decoding is short and reports no progress; analysis drives the bar;
    // installing means it's all but done.
    final value = switch (stage) {
      LoadStage.decoding => 0.0,
      LoadStage.analyzing => progress.clamp(0.0, 1.0),
      LoadStage.installing || LoadStage.done => 1.0,
    };
    final stageText = cancelling
        ? l.cancelling
        : switch (stage) {
            LoadStage.decoding => l.loadStageDecode,
            LoadStage.analyzing => l.loadStageAnalyze,
            LoadStage.installing || LoadStage.done => l.loadStageRestore,
          };
    final hint = reanalyze
        ? null
        : hasCache
        ? l.loadHintCached
        : l.loadHintFirst;
    return Container(
      padding: const EdgeInsets.fromLTRB(16, 16, 16, 14),
      decoration: BoxDecoration(
        color: CantoColors.raised,
        borderRadius: BorderRadius.circular(CantoRadii.card),
        border: Border.all(color: CantoColors.line),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        mainAxisSize: MainAxisSize.min,
        children: [
          Row(
            children: [
              const Icon(
                LucideIcons.audioWaveform,
                size: 18,
                color: CantoColors.muted,
              ),
              const SizedBox(width: 10),
              Expanded(
                child: Text(
                  fileName,
                  key: const Key('load-file'),
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: CantoTypography.sans(
                    size: 14,
                    weight: FontWeight.w600,
                    color: CantoColors.inkBright,
                  ),
                ),
              ),
              const SizedBox(width: 10),
              Text(
                '${(value * 100).round()}%',
                key: const Key('load-percent'),
                style: CantoTypography.num(
                  size: 13,
                  weight: FontWeight.w600,
                  color: CantoColors.ink,
                ),
              ),
            ],
          ),
          const SizedBox(height: 12),
          _Bar(value: value),
          const SizedBox(height: 10),
          Text(
            stageText,
            key: const Key('load-stage'),
            style: CantoTypography.sans(
              size: 12.5,
              weight: FontWeight.w600,
              color: CantoColors.ink,
            ),
          ),
          if (hint != null) ...[
            const SizedBox(height: 4),
            Text(
              hint,
              style: CantoTypography.sans(
                size: 11.5,
                color: CantoColors.muted,
                height: 1.4,
              ),
            ),
          ],
          if (onCancel != null) ...[
            const SizedBox(height: 14),
            CantoButton(
              key: const Key('load-cancel'),
              label: l.cancel,
              height: 40,
              expand: true,
              style: CantoButtonStyle.outline,
              onTap: cancelling ? null : onCancel,
            ),
          ],
        ],
      ),
    );
  }
}

/// Determinate bar; moves smoothly between the worker's progress reports.
class _Bar extends StatelessWidget {
  final double value;

  const _Bar({required this.value});

  @override
  Widget build(BuildContext context) {
    return ClipRRect(
      borderRadius: BorderRadius.circular(3),
      child: SizedBox(
        height: 6,
        child: TweenAnimationBuilder<double>(
          tween: Tween(end: value),
          duration: const Duration(milliseconds: 180),
          builder: (context, v, _) => Stack(
            fit: StackFit.expand,
            children: [
              const ColoredBox(color: CantoColors.high),
              FractionallySizedBox(
                alignment: Alignment.centerLeft,
                widthFactor: v,
                child: const ColoredBox(color: CantoColors.accent),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

/// Full screen shown while a project opens: what's opening and how far.
class ProjectLoadingView extends StatelessWidget {
  final String projectName;
  final int fileCount;
  final Widget card;

  const ProjectLoadingView({
    super.key,
    required this.projectName,
    required this.fileCount,
    required this.card,
  });

  @override
  Widget build(BuildContext context) {
    final l = AppLocalizations.of(context);
    // Landscape phones: drop the logo so Cancel stays on screen.
    final short = MediaQuery.sizeOf(context).height < 500;
    return Scaffold(
      key: const Key('project-loading'),
      backgroundColor: CantoColors.surface,
      body: SafeArea(
        child: Center(
          child: SingleChildScrollView(
            padding: const EdgeInsets.symmetric(horizontal: 24, vertical: 20),
            child: ConstrainedBox(
              constraints: const BoxConstraints(maxWidth: 420),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                mainAxisSize: MainAxisSize.min,
                children: [
                  if (!short) ...[
                    const Center(child: CantoLogo(size: 52)),
                    const SizedBox(height: 20),
                  ],
                  Text(
                    l.loadingProject.toUpperCase(),
                    textAlign: TextAlign.center,
                    style: CantoTypography.label(text: l.loadingProject),
                  ),
                  const SizedBox(height: 6),
                  Text(
                    projectName,
                    textAlign: TextAlign.center,
                    maxLines: 2,
                    overflow: TextOverflow.ellipsis,
                    style: CantoTypography.sans(
                      size: 22,
                      weight: FontWeight.w700,
                      color: CantoColors.inkBright,
                    ),
                  ),
                  const SizedBox(height: 4),
                  Text(
                    l.projectFiles(fileCount),
                    textAlign: TextAlign.center,
                    style: CantoTypography.sans(
                      size: 12.5,
                      color: CantoColors.muted,
                    ),
                  ),
                  SizedBox(height: short ? 16 : 26),
                  card,
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }
}
