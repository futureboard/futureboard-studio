import 'package:flutter/widgets.dart';

import 'prefs.dart';

/// App language: `null` follows the device; otherwise a fixed locale.
class LocaleController extends ValueNotifier<Locale?> {
  static const List<String> supported = ['en', 'th'];

  LocaleController() : super(null);

  Future<void> load() async {
    final code = await AppPrefs.getString(AppPrefs.localeKey);
    if (code != null && supported.contains(code)) value = Locale(code);
  }

  void select(Locale? locale) {
    value = locale;
    AppPrefs.setString(AppPrefs.localeKey, locale?.languageCode);
  }
}

/// Default pitch-snap mode for the editor ('off' | 'semi' | 'scale').
class SnapDefaultController extends ValueNotifier<String> {
  static const List<String> modes = ['off', 'semi', 'scale'];

  SnapDefaultController() : super('scale');

  Future<void> load() async {
    final mode = await AppPrefs.getString(AppPrefs.snapKey);
    if (mode != null && modes.contains(mode)) value = mode;
  }

  void select(String mode) {
    if (!modes.contains(mode)) return;
    value = mode;
    AppPrefs.setString(AppPrefs.snapKey, mode);
  }
}

/// Exposes the app-wide settings controllers to the widget tree.
class SettingsScope extends InheritedWidget {
  final LocaleController locale;
  final SnapDefaultController snapDefault;

  const SettingsScope({
    super.key,
    required this.locale,
    required this.snapDefault,
    required super.child,
  });

  static SettingsScope of(BuildContext context) =>
      context.dependOnInheritedWidgetOfExactType<SettingsScope>()!;

  @override
  bool updateShouldNotify(SettingsScope old) =>
      locale != old.locale || snapDefault != old.snapDefault;
}
