import 'package:flutter/material.dart';

/// Futureboard "Default" theme (`packages/shared/themes/Default.json`,
/// `futureboard.default`) mapped onto the app's roles. Theme JSON stores
/// #RRGGBBAA; Flutter's Color takes 0xAARRGGBB.
class CantoColors {
  // Surfaces, darkest to brightest (DESIGN.md surface order).
  static const Color bg = Color(0xFF161616); // surface.canvas
  static const Color well = Color(0xFF1B1B1B); // timeline.background
  static const Color surface = Color(0xFF1E1E1E); // surface.titlebar/sidebar
  static const Color raised = Color(0xFF2A2A2A); // surface.panel/card
  static const Color high = Color(0xFF3D3D3D); // surface.hover
  static const Color selected = Color(0xFF4A4A4A); // surface.selected

  static const Color line = Color(0x1FFFFFFF); // border.normal
  static const Color lineStrong = Color(0x33FFFFFF); // border.strong

  static const Color ink = Color(0xFFE8E8E8); // text.primary
  static const Color inkBright = Color(0xFFE8E8E8); // text.primary
  static const Color muted = Color(0xFF8F8F8F); // text.muted
  static const Color faint = Color(0xFF6E6E6E); // text.faint
  static const Color canvasMuted = Color(0xFFBDBDBD); // text.secondary
  static const Color canvasFaint = Color(0xFF5A5A5A); // text.disabled

  static const Color accent = Color(0xFF4D9CF8); // accent.primary
  static const Color accentHover = Color(0xFF72B1FA); // accent.hover
  static const Color accentInk = Color(0xFF07121F); // accent.onAccent
  static const Color accentSoft = Color(0x294D9CF8); // accent.soft
  static const Color accentLine = Color(0x8C4D9CF8); // border.accent

  static const Color heat = Color(0xFFF2645F); // status.error
  static const Color heatSoft = Color(0x29F2645F);
  static const Color success = Color(0xFF43D18A); // status.success

  // Bypass is a latched toggle: its own colour, never the accent.
  static const Color bypass = Color(0xFFE8B75C); // status.warning
  static const Color bypassSoft = Color(0x29E8B75C);
  static const Color bypassLine = Color(0x8CE8B75C);

  static const Color playhead = Color(0xFFFF6A5A); // timeline.playhead

  // Timeline / editor canvas.
  static const Color rulerBg = Color(0xFF2A2A2A); // timeline.rulerBackground
  static const Color rulerText = Color(0xFFD2D2D2); // timeline.rulerText
  static const Color rulerTick = Color(0x45FFFFFF); // timeline.rulerTick
  static const Color gridMinor = Color(0x0DFFFFFF); // timeline.gridMinor
  static const Color gridMajor = Color(0x1CFFFFFF); // timeline.gridMajor
  static const Color gridBar = Color(0x3DFFFFFF); // timeline.gridBar
  static const Color laneRow = Color(0x07FFFFFF); // timeline.laneBackground

  // Piano keyboard (pitch ruler).
  static const Color keyWhite = Color(0xFFE6E6E6); // piano.whiteKey
  static const Color keyBlack = Color(0xFF161616); // piano.blackKey
  static const Color keySeam = Color(0xFFA6A6A6); // piano.keySeam
  static const Color keyLabel = Color(0xFF4C4C4C); // piano.keyLabel

  // Meter.
  static const Color meterLow = Color(0xFF43D18A); // meter.low
  static const Color meterMid = Color(0xFFE8B75C); // meter.mid
  static const Color meterHigh = Color(0xFFF2645F); // meter.high
}

class CantoRadii {
  static const double chip = 8;
  static const double card = 12;
  static const double sheet = 20;
}

class CantoTypography {
  /// UI text: Mona Sans variable font (wght 200–900).
  static const String sansFamily = 'MonaSans';

  /// Readouts (notes, cents, Hz, clock): Barlow static weights with tabular
  /// figures so changing values don't jitter.
  static const String numFamily = 'Barlow';

  /// Thai glyphs come from Google Sans (bundled Thai subset); Latin stays in
  /// Mona Sans / Barlow and Thai runs fall through to it.
  static const List<String> fallback = ['GoogleSans'];

  static TextStyle sans({
    double size = 12,
    FontWeight weight = FontWeight.w500,
    Color color = CantoColors.ink,
    double? letterSpacing,
    double? height,
  }) {
    final wght = weight.value.toDouble();
    return TextStyle(
      fontFamily: sansFamily,
      fontFamilyFallback: fallback,
      fontSize: size,
      fontWeight: weight,
      fontVariations: [FontVariation('wght', wght)],
      color: color,
      letterSpacing: letterSpacing,
      height: height,
    );
  }

  static TextStyle num({
    double size = 12,
    FontWeight weight = FontWeight.w500,
    Color color = CantoColors.ink,
    double? letterSpacing,
    double? height,
    bool tabular = true,
  }) {
    return TextStyle(
      fontFamily: numFamily,
      fontFamilyFallback: fallback,
      fontSize: size,
      fontWeight: weight,
      fontFeatures: tabular ? const [FontFeature.tabularFigures()] : null,
      color: color,
      letterSpacing: letterSpacing,
      height: height,
    );
  }

  /// Small uppercase section label.
  static final RegExp _thai = RegExp('[฀-๿]');

  /// Small section label. Tracking suits upper-case Latin but pulls Thai
  /// vowel and tone marks away from their consonants, so [text] in Thai gets
  /// none.
  static TextStyle label({
    Color color = CantoColors.faint,
    double size = 9.5,
    String text = '',
  }) {
    return sans(
      size: size,
      weight: FontWeight.w600,
      color: color,
      letterSpacing: _thai.hasMatch(text) ? 0 : 0.9,
    );
  }
}
