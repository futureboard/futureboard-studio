import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../l10n/app_localizations.dart';
import '../license.dart';
import '../theme.dart';
import '../ui/components.dart';

/// Shown instead of the app when Google Play answers that this account has
/// not bought it: a way to the store page, and a way to ask again once it
/// has.
class NotLicensedPage extends StatelessWidget {
  final VoidCallback onRetry;
  final bool checking;

  const NotLicensedPage({
    super.key,
    required this.onRetry,
    this.checking = false,
  });

  @override
  Widget build(BuildContext context) {
    final l = AppLocalizations.of(context);
    final side = math.max(24.0, (MediaQuery.sizeOf(context).width - 480) / 2);
    return Scaffold(
      backgroundColor: CantoColors.surface,
      body: SafeArea(
        child: Center(
          child: SingleChildScrollView(
            padding: EdgeInsets.symmetric(horizontal: side, vertical: 24),
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                const Icon(
                  LucideIcons.circleAlert,
                  size: 36,
                  color: CantoColors.heat,
                ),
                const SizedBox(height: 16),
                Text(
                  l.licenseTitle,
                  key: const Key('license-title'),
                  textAlign: TextAlign.center,
                  style: CantoTypography.sans(
                    size: 20,
                    weight: FontWeight.w600,
                    color: CantoColors.inkBright,
                  ),
                ),
                const SizedBox(height: 10),
                Text(
                  l.licenseBody,
                  textAlign: TextAlign.center,
                  style: CantoTypography.sans(
                    size: 14,
                    color: CantoColors.muted,
                    height: 1.5,
                  ),
                ),
                const SizedBox(height: 24),
                CantoButton(
                  key: const Key('license-open-store'),
                  label: l.licenseOpenStore,
                  icon: LucideIcons.fileDown,
                  style: CantoButtonStyle.primary,
                  height: 46,
                  expand: true,
                  onTap: PlayLicense.openStore,
                ),
                const SizedBox(height: 10),
                CantoButton(
                  key: const Key('license-retry'),
                  label: l.licenseRetry,
                  icon: LucideIcons.rotateCcw,
                  style: CantoButtonStyle.outline,
                  height: 46,
                  expand: true,
                  onTap: checking ? null : onRetry,
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}
