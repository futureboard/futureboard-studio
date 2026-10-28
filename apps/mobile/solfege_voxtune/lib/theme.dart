import 'package:flutter/material.dart';

class CantoColors {
  static const Color desk = Color(0xFF090908);
  static const Color panel = Color(0xFF121110);
  static const Color well = Color(0xFF191816);
  static const Color line = Color(0xFF2A2722);
  static const Color ink = Color(0xFFECE7DE);
  static const Color inkBright = Color(0xFFF4EFE6);
  static const Color muted = Color(0xFF8F8B82);
  static const Color canvasMuted = Color(0xFF9A958C);
  static const Color faint = Color(0xFF6D6962);
  static const Color canvasFaint = Color(0xFF5E5A54);
  static const Color accent = Color(0xFFE39A62);
  static const Color heat = Color(0xFFD67264);
  static const Color playhead = Color(0xFFFF6437);
  static const Color transportBg = Color(0xFF10100E);
}

class CantoTypography {
  static const String sansFamily = 'SpaceGrotesk';
  static const String monoFamily = 'JetBrainsMono';

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
      fontSize: size,
      fontWeight: weight,
      fontVariations: [FontVariation('wght', wght)],
      color: color,
      letterSpacing: letterSpacing,
      height: height,
    );
  }

  static TextStyle mono({
    double size = 12,
    FontWeight weight = FontWeight.w500,
    Color color = CantoColors.ink,
    double? letterSpacing,
    double? height,
    bool tabular = true,
  }) {
    final wght = weight.value.toDouble();
    return TextStyle(
      fontFamily: monoFamily,
      fontSize: size,
      fontWeight: weight,
      fontVariations: [FontVariation('wght', wght)],
      fontFeatures: tabular ? const [FontFeature.tabularFigures()] : null,
      color: color,
      letterSpacing: letterSpacing,
      height: height,
    );
  }
}
