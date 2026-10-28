import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../cache.dart';
import '../l10n/app_localizations.dart';
import '../locale_controller.dart';
import '../project.dart';
import '../theme.dart';
import '../ui/components.dart';

/// Shown in Settings → About; keep in step with `version:` in pubspec.yaml.
const String kAppVersion = '1.0.0';

class SettingsPage extends StatelessWidget {
  final ProjectStore store;

  const SettingsPage({super.key, required this.store});

  @override
  Widget build(BuildContext context) {
    final l = AppLocalizations.of(context);
    final settings = SettingsScope.of(context);
    // Tablets and landscape: a readable column instead of full-width cards.
    final side = math.max(16.0, (MediaQuery.sizeOf(context).width - 720) / 2);
    return Scaffold(
      backgroundColor: CantoColors.surface,
      body: SafeArea(
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            SizedBox(
              height: 56,
              child: Row(
                children: [
                  const SizedBox(width: 4),
                  CantoIconButton(
                    key: const Key('settings-back'),
                    icon: LucideIcons.arrowLeft,
                    tooltip: MaterialLocalizations.of(context)
                        .backButtonTooltip,
                    onTap: () => Navigator.of(context).pop(),
                  ),
                  const SizedBox(width: 4),
                  Text(
                    l.settings,
                    style: CantoTypography.sans(
                      size: 18,
                      weight: FontWeight.w600,
                      color: CantoColors.inkBright,
                    ),
                  ),
                ],
              ),
            ),
            Expanded(
              child: ListView(
                padding: EdgeInsets.fromLTRB(side, 4, side, 24),
                children: [
                  _Section(
                    icon: LucideIcons.languages,
                    title: l.language,
                    child: ValueListenableBuilder<Locale?>(
                      valueListenable: settings.locale,
                      builder: (_, locale, _) => CantoSegmented<String>(
                        key: const Key('language-picker'),
                        selected: locale?.languageCode ?? 'system',
                        onChanged: (code) => settings.locale.select(
                          code == 'system' ? null : Locale(code),
                        ),
                        options: [
                          SegmentOption(
                            'system',
                            l.languageSystem,
                            key: const Key('lang-system'),
                          ),
                          // Language names stay in their own language.
                          const SegmentOption(
                            'en',
                            'English',
                            key: Key('lang-en'),
                          ),
                          const SegmentOption('th', 'ไทย', key: Key('lang-th')),
                        ],
                      ),
                    ),
                  ),
                  _Section(
                    icon: LucideIcons.magnet,
                    title: l.defaultSnap,
                    subtitle: l.defaultSnapHint,
                    child: ValueListenableBuilder<String>(
                      valueListenable: settings.snapDefault,
                      builder: (_, mode, _) => CantoSegmented<String>(
                        key: const Key('snap-default-picker'),
                        selected: mode,
                        onChanged: settings.snapDefault.select,
                        options: [
                          SegmentOption('off', l.snapFree),
                          SegmentOption('semi', l.snapSemi),
                          SegmentOption('scale', l.snapScale),
                        ],
                      ),
                    ),
                  ),
                  _Section(
                    icon: LucideIcons.folderOpen,
                    title: l.storage,
                    subtitle: l.storageHint,
                    child: SelectableText(
                      store.root,
                      key: const Key('storage-path'),
                      style: CantoTypography.num(
                        size: 12,
                        color: CantoColors.canvasMuted,
                      ),
                    ),
                  ),
                  _Section(
                    icon: LucideIcons.databaseZap,
                    title: l.cache,
                    subtitle: l.cacheHint,
                    child: _CacheControl(store: store),
                  ),
                  _Section(
                    icon: LucideIcons.info,
                    title: l.about,
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Text(
                          '${l.appName} · ${l.version(kAppVersion)}',
                          style: CantoTypography.sans(
                            size: 13,
                            weight: FontWeight.w600,
                            color: CantoColors.ink,
                          ),
                        ),
                        const SizedBox(height: 6),
                        Text(
                          l.aboutBody,
                          style: CantoTypography.sans(
                            size: 12,
                            color: CantoColors.muted,
                            height: 1.45,
                          ),
                        ),
                      ],
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

/// Live size of the analysis/history cache with a Clear button.
class _CacheControl extends StatefulWidget {
  final ProjectStore store;

  const _CacheControl({required this.store});

  @override
  State<_CacheControl> createState() => _CacheControlState();
}

class _CacheControlState extends State<_CacheControl> {
  late int _bytes = ProjectCache.totalBytes(widget.store);

  void _clear() {
    ProjectCache.clearAll(widget.store);
    setState(() => _bytes = ProjectCache.totalBytes(widget.store));
    ScaffoldMessenger.of(context).showSnackBar(
      SnackBar(content: Text(AppLocalizations.of(context).cacheCleared)),
    );
  }

  @override
  Widget build(BuildContext context) {
    final l = AppLocalizations.of(context);
    return Row(
      children: [
        Expanded(
          child: Text(
            l.cacheUsed(formatBytes(_bytes)),
            key: const Key('cache-size'),
            style: CantoTypography.num(
              size: 13,
              weight: FontWeight.w600,
              color: CantoColors.ink,
            ),
          ),
        ),
        CantoButton(
          key: const Key('cache-clear'),
          label: l.clearCache,
          icon: LucideIcons.trash2,
          height: 34,
          style: CantoButtonStyle.outline,
          onTap: _bytes > 0 ? _clear : null,
        ),
      ],
    );
  }
}

class _Section extends StatelessWidget {
  final IconData icon;
  final String title;
  final String? subtitle;
  final Widget child;

  const _Section({
    required this.icon,
    required this.title,
    this.subtitle,
    required this.child,
  });

  @override
  Widget build(BuildContext context) {
    return Container(
      margin: const EdgeInsets.only(top: 10),
      padding: const EdgeInsets.all(14),
      decoration: BoxDecoration(
        color: CantoColors.raised,
        borderRadius: BorderRadius.circular(CantoRadii.card),
        border: Border.all(color: CantoColors.line),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Icon(icon, size: 16, color: CantoColors.muted),
              const SizedBox(width: 8),
              Expanded(
                child: Text(
                  title,
                  style: CantoTypography.sans(
                    size: 13.5,
                    weight: FontWeight.w600,
                    color: CantoColors.inkBright,
                  ),
                ),
              ),
            ],
          ),
          if (subtitle != null) ...[
            const SizedBox(height: 3),
            Padding(
              padding: const EdgeInsets.only(left: 24),
              child: Text(
                subtitle!,
                style: CantoTypography.sans(
                  size: 11.5,
                  color: CantoColors.muted,
                ),
              ),
            ),
          ],
          const SizedBox(height: 12),
          child,
        ],
      ),
    );
  }
}
