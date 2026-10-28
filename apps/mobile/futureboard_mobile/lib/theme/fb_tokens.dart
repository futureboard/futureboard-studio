// Exact Flutter port of crates/SphereUIComponents/src/theme.rs,
// packages/shared/themes/Default.json & Light.json, and DESIGN.md tokens.

import 'package:flutter/material.dart';

/// Corner-radius scale (`DESIGN.md` -> "Radius Tokens").
/// Note: timeline grid cells, track lanes, ruler ticks, meter bars, and canvas
/// regions intentionally stay square (`FbRadius.none`).
class FbRadius {
  FbRadius._();
  static const double none = 0.0;
  static const double xs = 3.0;
  static const double sm = 4.0;
  static const double md = 6.0;
  static const double lg = 8.0;
  static const double xl = 12.0;
  static const double pill = 999.0;
}

/// 4 px spacing scale (`DESIGN.md` -> "Spacing Scale").
class FbSpace {
  FbSpace._();
  static const double s0 = 0.0;
  static const double s1 = 2.0;
  static const double s2 = 4.0;
  static const double s3 = 8.0;
  static const double s4 = 12.0;
  static const double s5 = 16.0;
  static const double s6 = 24.0;
  static const double s7 = 32.0;
}

/// Component height and shell geometry tokens (`DESIGN.md` -> "Sizing").
class FbSize {
  FbSize._();
  static const double titlebarH = 38.0;
  static const double transportH = 44.0;
  static const double statusbarH = 26.0;
  static const double controlSm = 24.0;
  static const double controlMd = 28.0;
  static const double controlLg = 32.0;
  static const double iconButton = 28.0;
  static const double latchButton = 22.0;
  static const double trackHeaderW = 196.0;
  static const double channelStripW = 84.0;
  static const double masterStripW = 112.0;
}

/// Interactive state-wash alpha scale (`DESIGN.md` -> "State Layer").
class FbState {
  FbState._();
  static const double rest = 0.0;
  static const double hover = 0.06;
  static const double pressed = 0.12;
  static const double selected = 0.16;
  static const double latchFill = 0.22;
  static const double latchBorder = 0.72;
  static const double disabled = 0.38;
  static const double focusRing = 0.65;
}

/// Semantic studio color tokens ported 1:1 from `crates/SphereUIComponents/src/theme.rs`
/// and `packages/shared/themes/Default.json` & `Light.json`.
class FbColors {
  final bool isDark;
  final Color bgCanvas;
  final Color bgPrimary;
  final Color bgSecondary;
  final Color bgTertiary;
  final Color bgElevated;
  final Color bgOverlay;
  final Color bgSunken;

  final Color borderSubtle;
  final Color borderDefault;
  final Color borderStrong;
  final Color borderFocus;

  final Color textPrimary;
  final Color textSecondary;
  final Color textMuted;
  final Color textAccent;
  final Color textInverse;

  final Color accentPrimary;
  final Color accentHover;
  final Color accentActive;
  final Color accentSoft;

  final Color transportPlay;
  final Color transportRecord;
  final Color transportStop;
  final Color transportLoop;

  // Latched channel states (two-channel encoding: fill wash + border + glyph)
  final Color stateMute;
  final Color stateSolo;
  final Color stateArm;
  final Color stateMonitor;
  final Color stateAuto;

  final Color meterGreen;
  final Color meterYellow;
  final Color meterOrange;
  final Color meterRed;

  final Color statusOk;
  final Color statusWarn;
  final Color statusError;

  final Color gridBarLine;
  final Color gridBeatLine;
  final Color gridSubLine;
  final Color playhead;

  const FbColors({
    required this.isDark,
    required this.bgCanvas,
    required this.bgPrimary,
    required this.bgSecondary,
    required this.bgTertiary,
    required this.bgElevated,
    required this.bgOverlay,
    required this.bgSunken,
    required this.borderSubtle,
    required this.borderDefault,
    required this.borderStrong,
    required this.borderFocus,
    required this.textPrimary,
    required this.textSecondary,
    required this.textMuted,
    required this.textAccent,
    required this.textInverse,
    required this.accentPrimary,
    required this.accentHover,
    required this.accentActive,
    required this.accentSoft,
    required this.transportPlay,
    required this.transportRecord,
    required this.transportStop,
    required this.transportLoop,
    required this.stateMute,
    required this.stateSolo,
    required this.stateArm,
    required this.stateMonitor,
    required this.stateAuto,
    required this.meterGreen,
    required this.meterYellow,
    required this.meterOrange,
    required this.meterRed,
    required this.statusOk,
    required this.statusWarn,
    required this.statusError,
    required this.gridBarLine,
    required this.gridBeatLine,
    required this.gridSubLine,
    required this.playhead,
  });

  /// Default Dark Studio theme (`packages/shared/themes/Default.json` + `theme.rs`).
  static const FbColors dark = FbColors(
    isDark: true,
    bgCanvas: Color(0xFF0A0D12),
    bgPrimary: Color(0xFF0F131A),
    bgSecondary: Color(0xFF151A24),
    bgTertiary: Color(0xFF1C2330),
    bgElevated: Color(0xFF232B3A),
    bgOverlay: Color(0xE6151A24),
    bgSunken: Color(0xFF07090D),
    borderSubtle: Color(0xFF1D2432),
    borderDefault: Color(0xFF283142),
    borderStrong: Color(0xFF38455C),
    borderFocus: Color(0xFF4C8DFF),
    textPrimary: Color(0xFFE6ECF5),
    textSecondary: Color(0xFF9AA6BA),
    textMuted: Color(0xFF636F82),
    textAccent: Color(0xFF6CA2FF),
    textInverse: Color(0xFF0A0D12),
    accentPrimary: Color(0xFF4C8DFF),
    accentHover: Color(0xFF669EFF),
    accentActive: Color(0xFF3674E0),
    accentSoft: Color(0x2E4C8DFF),
    transportPlay: Color(0xFF2ECC71),
    transportRecord: Color(0xFFEF4444),
    transportStop: Color(0xFF9AA6BA),
    transportLoop: Color(0xFF4C8DFF),
    stateMute: Color(0xFFE69F00),    // Okabe-Ito Amber for Mute
    stateSolo: Color(0xFF56B4E9),    // Okabe-Ito Sky Cyan for Solo
    stateArm: Color(0xFFEF4444),     // Crimson for Record Arm
    stateMonitor: Color(0xFF2ECC71), // Emerald for Input Monitor
    stateAuto: Color(0xFFA855F7),    // Violet for Automation Read
    meterGreen: Color(0xFF2ECC71),
    meterYellow: Color(0xFFFACC15),
    meterOrange: Color(0xFFF97316),
    meterRed: Color(0xFFEF4444),
    statusOk: Color(0xFF2ECC71),
    statusWarn: Color(0xFFF59E0B),
    statusError: Color(0xFFEF4444),
    gridBarLine: Color(0xFF2C364A),
    gridBeatLine: Color(0xFF1B2230),
    gridSubLine: Color(0xFF131822),
    playhead: Color(0xFFF8FAFC),
  );

  /// Light Studio theme (`packages/shared/themes/Light.json`).
  static const FbColors light = FbColors(
    isDark: false,
    bgCanvas: Color(0xFFE9EDF4),
    bgPrimary: Color(0xFFF3F6FA),
    bgSecondary: Color(0xFFFFFFFF),
    bgTertiary: Color(0xFFE4E9F2),
    bgElevated: Color(0xFFFFFFFF),
    bgOverlay: Color(0xF2FFFFFF),
    bgSunken: Color(0xFFDCE2EC),
    borderSubtle: Color(0xFFDCE2EC),
    borderDefault: Color(0xFFC6D0E0),
    borderStrong: Color(0xFFA3B1C6),
    borderFocus: Color(0xFF2563EB),
    textPrimary: Color(0xFF111827),
    textSecondary: Color(0xFF4B5563),
    textMuted: Color(0xFF6B7280),
    textAccent: Color(0xFF2563EB),
    textInverse: Color(0xFFFFFFFF),
    accentPrimary: Color(0xFF2563EB),
    accentHover: Color(0xFF1D4ED8),
    accentActive: Color(0xFF1E40AF),
    accentSoft: Color(0x262563EB),
    transportPlay: Color(0xFF16A34A),
    transportRecord: Color(0xFFDC2626),
    transportStop: Color(0xFF4B5563),
    transportLoop: Color(0xFF2563EB),
    stateMute: Color(0xFFD97706),
    stateSolo: Color(0xFF0284C7),
    stateArm: Color(0xFFDC2626),
    stateMonitor: Color(0xFF16A34A),
    stateAuto: Color(0xFF9333EA),
    meterGreen: Color(0xFF16A34A),
    meterYellow: Color(0xFFEAB308),
    meterOrange: Color(0xFFEA580C),
    meterRed: Color(0xFFDC2626),
    statusOk: Color(0xFF16A34A),
    statusWarn: Color(0xFFD97706),
    statusError: Color(0xFFDC2626),
    gridBarLine: Color(0xFFCBD5E1),
    gridBeatLine: Color(0xFFE2E8F0),
    gridSubLine: Color(0xFFF1F5F9),
    playhead: Color(0xFF0F172A),
  );

  static Color parseHex(String hex, [Color fallback = const Color(0xFF4C8DFF)]) {
    final cleaned = hex.replaceAll('#', '').trim();
    if (cleaned.length == 6) {
      final v = int.tryParse('FF$cleaned', radix: 16);
      if (v != null) return Color(v);
    } else if (cleaned.length == 8) {
      final v = int.tryParse(cleaned, radix: 16);
      if (v != null) return Color(v);
    }
    return fallback;
  }
}

/// Typography tokens with enforced `FontFeature.tabularFigures()` on all numeric readouts
/// (`DESIGN.md` -> "Typography & Tabular Readouts").
class FbTypography {
  FbTypography._();

  static const String fontSans = 'SpaceGrotesk';
  static const String fontMono = 'JetBrainsMono';

  static TextStyle sectionHeader(FbColors c) => TextStyle(
        fontFamily: fontSans,
        fontSize: 10.5,
        fontWeight: FontWeight.w700,
        letterSpacing: 0.9,
        color: c.textMuted,
      );

  static TextStyle title(FbColors c) => TextStyle(
        fontFamily: fontSans,
        fontSize: 13.0,
        fontWeight: FontWeight.w600,
        color: c.textPrimary,
      );

  static TextStyle body(FbColors c) => TextStyle(
        fontFamily: fontSans,
        fontSize: 12.0,
        fontWeight: FontWeight.w500,
        color: c.textPrimary,
      );

  static TextStyle label(FbColors c) => TextStyle(
        fontFamily: fontSans,
        fontSize: 11.0,
        fontWeight: FontWeight.w500,
        color: c.textSecondary,
      );

  static TextStyle micro(FbColors c) => TextStyle(
        fontFamily: fontSans,
        fontSize: 10.0,
        fontWeight: FontWeight.w500,
        color: c.textMuted,
      );

  static TextStyle monoReadout(FbColors c, {double size = 12.0, Color? color}) =>
      TextStyle(
        fontFamily: fontMono,
        fontSize: size,
        fontWeight: FontWeight.w600,
        color: color ?? c.textPrimary,
        fontFeatures: const [FontFeature.tabularFigures()],
      );

  static TextStyle transportClock(FbColors c, {Color? color}) => TextStyle(
        fontFamily: fontMono,
        fontSize: 15.0,
        fontWeight: FontWeight.w700,
        letterSpacing: 0.4,
        color: color ?? c.textPrimary,
        fontFeatures: const [FontFeature.tabularFigures()],
      );
}
