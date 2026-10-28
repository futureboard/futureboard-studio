import 'package:flutter/services.dart';

/// Small persisted settings, stored in Android SharedPreferences through the
/// `canto/prefs` channel. Where the channel is missing (tests, desktop)
/// reads return null and writes are dropped.
class AppPrefs {
  static const MethodChannel _channel = MethodChannel('canto/prefs');

  static const String localeKey = 'locale';
  static const String snapKey = 'snap_default';

  static Future<String?> getString(String key) async {
    try {
      return await _channel.invokeMethod<String>('getString', {'key': key});
    } catch (_) {
      return null;
    }
  }

  static void setString(String key, String? value) {
    try {
      _channel
          .invokeMethod('setString', {'key': key, 'value': value})
          .catchError((_) => null);
    } catch (_) {}
  }
}
