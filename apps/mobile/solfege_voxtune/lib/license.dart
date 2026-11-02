import 'package:flutter/services.dart';

/// Google Play's answer on whether this copy was bought.
enum LicenseState {
  licensed,
  notLicensed,

  /// No answer (offline, no Play Store, an error) and none remembered.
  unknown,

  /// The check is off: the build has no licensing key (`.env`).
  disabled,
}

/// The purchase check, through the `canto/license` channel (PlayLicensing
/// on Android). Where the channel is missing (tests, desktop) the answer is
/// [LicenseState.unknown], which does not block anything.
class PlayLicense {
  static const MethodChannel _channel = MethodChannel('canto/license');

  static Future<LicenseState> check() async {
    try {
      final wire = await _channel.invokeMethod<String>('check');
      return switch (wire) {
        'licensed' => LicenseState.licensed,
        'not_licensed' => LicenseState.notLicensed,
        'disabled' => LicenseState.disabled,
        _ => LicenseState.unknown,
      };
    } catch (_) {
      return LicenseState.unknown;
    }
  }

  /// Opens the app's Google Play page.
  static Future<void> openStore() async {
    try {
      await _channel.invokeMethod('openStore');
    } catch (_) {}
  }
}
