import 'dart:math' as math;
import 'dart:ui' as ui;
import 'package:flutter/material.dart';
import 'music.dart';
import 'theme.dart';

class EditorLayout {
  final double w;
  final double h;
  final double padL;
  final double padT;
  final double padB;
  final double padR;
  final double gridW;
  final double gridH;
  final double pxPerSemi;
  final double pxPerSec;

  const EditorLayout({
    required this.w,
    required this.h,
    required this.padL,
    required this.padT,
    required this.padB,
    required this.padR,
    required this.gridW,
    required this.gridH,
    required this.pxPerSemi,
    required this.pxPerSec,
  });

  factory EditorLayout.compute(
    double width,
    double height, {
    required double pitchLo,
    required double pitchHi,
    required double viewDuration,
  }) {
    const padL = 36.0;
    const padT = 20.0;
    const padB = 18.0;
    const padR = 6.0;
    final gridW = math.max(1.0, width - padL - padR);
    final gridH = math.max(1.0, height - padT - padB);
    final pxPerSemi = gridH / math.max(1.0, pitchHi - pitchLo);
    final pxPerSec = gridW / math.max(0.25, viewDuration);
    return EditorLayout(
      w: width,
      h: height,
      padL: padL,
      padT: padT,
      padB: padB,
      padR: padR,
      gridW: gridW,
      gridH: gridH,
      pxPerSemi: pxPerSemi,
      pxPerSec: pxPerSec,
    );
  }

  double timeToX(double viewStart, double time) {
    return padL + (time - viewStart) * pxPerSec;
  }

  double xToTime(double viewStart, double x) {
    return viewStart + (x - padL) / pxPerSec;
  }

  double pitchToY(double pitchHi, double pitch) {
    return padT + (pitchHi - pitch) * pxPerSemi;
  }

  double yToPitch(double pitchHi, double y) {
    return pitchHi - (y - padT) / pxPerSemi;
  }
}

class HitResult {
  final String type; // 'ruler', 'pitch-ruler', 'note', 'empty'
  final VocalNote? note;
  final String edge; // 'body', 'start', 'end'
  final EditorLayout layout;

  const HitResult({
    required this.type,
    this.note,
    this.edge = 'body',
    required this.layout,
  });
}

HitResult hitTestEditor({
  required double x,
  required double y,
  required double width,
  required double height,
  required List<VocalNote> notes,
  required String? selectedId,
  required String tool,
  required bool bypass,
  required double viewStart,
  required double viewDuration,
  required double pitchLo,
  required double pitchHi,
}) {
  final layout = EditorLayout.compute(
    width,
    height,
    pitchLo: pitchLo,
    pitchHi: pitchHi,
    viewDuration: viewDuration,
  );
  final gridBottom = layout.h - layout.padB;
  if (y <= layout.padT && x >= layout.padL) {
    return HitResult(type: 'ruler', layout: layout);
  }
  if (x < layout.padL && y > layout.padT && y < gridBottom) {
    return HitResult(type: 'pitch-ruler', layout: layout);
  }
  if (y >= layout.padT && y <= gridBottom && x >= layout.padL) {
    final selectedFirst = [...notes]..sort((a, b) {
        if (a.id == selectedId) return -1;
        if (b.id == selectedId) return 1;
        return b.start.compareTo(a.start);
      });
    for (final note in selectedFirst) {
      final shown = presentNote(note, bypass);
      final hit = _noteHit(
        x: x,
        y: y,
        shown: shown,
        note: note,
        layout: layout,
        viewStart: viewStart,
        pitchHi: pitchHi,
        tool: tool,
        gridBottom: gridBottom,
      );
      if (hit != null) return hit;
    }
  }
  if (y > gridBottom) {
    for (final note in notes.reversed) {
      final shown = presentNote(note, bypass);
      final x0 = layout.timeToX(viewStart, shown.start);
      final x1 = layout.timeToX(viewStart, shown.start + shown.dur);
      if (x >= x0 && x <= x1) {
        return HitResult(type: 'note', note: note, edge: 'body', layout: layout);
      }
    }
  }
  return HitResult(type: 'empty', layout: layout);
}

HitResult? _noteHit({
  required double x,
  required double y,
  required VocalNote shown,
  required VocalNote note,
  required EditorLayout layout,
  required double viewStart,
  required double pitchHi,
  required String tool,
  required double gridBottom,
}) {
  final x0 = layout.timeToX(viewStart, shown.start);
  final x1 = layout.timeToX(viewStart, shown.start + shown.dur);
  if (x < x0 - 8 || x > x1 + 8 || y > gridBottom) return null;
  final span = math.max(0.0001, shown.dur);
  final time = layout.xToTime(viewStart, math.min(x1, math.max(x0, x)));
  final t = clampVal((time - shown.start) / span, 0.0, 1.0);
  final boxCenterY = layout.pitchToY(pitchHi, shown.pitch);
  final curveY = layout.pitchToY(pitchHi, pitchAt(shown, t));
  final boxHalf = math.max(13.0, layout.pxPerSemi * 0.44 + 5.0);
  final hitBox = (y - boxCenterY).abs() <= boxHalf;
  final hitCurve = (y - curveY).abs() <= math.max(10.0, layout.pxPerSemi * 0.35);
  if (!hitBox && !hitCurve) return null;
  String edge = 'body';
  if (tool == 'time' && !note.unpitched && x1 - x0 >= 38) {
    if ((x - x0).abs() < 14) {
      edge = 'start';
    } else if ((x - x1).abs() < 14) {
      edge = 'end';
    }
  }
  return HitResult(type: 'note', note: note, edge: edge, layout: layout);
}

Offset noteAnchorOffset({
  required VocalNote note,
  required double width,
  required double height,
  required bool bypass,
  required double viewStart,
  required double viewDuration,
  required double pitchLo,
  required double pitchHi,
}) {
  final layout = EditorLayout.compute(
    width,
    height,
    pitchLo: pitchLo,
    pitchHi: pitchHi,
    viewDuration: viewDuration,
  );
  final shown = presentNote(note, bypass);
  return Offset(
    layout.timeToX(viewStart, shown.start + shown.dur / 2.0),
    layout.pitchToY(pitchHi, shown.pitch),
  );
}

List<int> _rgbFor(int cents) {
  final absCents = cents.abs();
  if (absCents <= 6) {
    // Crisp in-tune warm amber-gold
    return [229, 156, 94];
  }
  final t = clampVal((absCents - 6.0) / 42.0, 0.0, 1.0);
  return [
    (229 + (214 - 229) * t).round(),
    (156 + (100 - 156) * t).round(),
    (94 + (82 - 94) * t).round(),
  ];
}

List<int> _lighten(List<int> rgb, double amount) {
  return rgb
      .map((channel) => (channel + (255 - channel) * amount).round())
      .toList();
}

int _samplesFor(VocalNote note) {
  return math.max(28, math.min(96, (note.dur * 72).round()));
}

void _curveThrough(Path path, List<Offset> points, {bool move = true}) {
  if (points.isEmpty) return;
  if (points.length < 3) {
    if (move) path.moveTo(points[0].dx, points[0].dy);
    for (int i = move ? 1 : 0; i < points.length; i += 1) {
      path.lineTo(points[i].dx, points[i].dy);
    }
    return;
  }
  int index = 0;
  if (move) {
    path.moveTo(points[0].dx, points[0].dy);
    index = 1;
  }
  for (; index < points.length - 1; index += 1) {
    final mx = (points[index].dx + points[index + 1].dx) / 2.0;
    final my = (points[index].dy + points[index + 1].dy) / 2.0;
    path.quadraticBezierTo(points[index].dx, points[index].dy, mx, my);
  }
  final last = points.last;
  path.lineTo(last.dx, last.dy);
}

class _BuiltNoteVisual {
  final RRect blockRRect;
  final RRect? ghostRRect;
  final Path energyPath;
  final List<Offset> curve;
  final List<Offset> ghostCurve;
  final double centerY;
  final double ghostCenterY;
  final bool hasGhost;

  const _BuiltNoteVisual({
    required this.blockRRect,
    required this.ghostRRect,
    required this.energyPath,
    required this.curve,
    required this.ghostCurve,
    required this.centerY,
    required this.ghostCenterY,
    required this.hasGhost,
  });
}

_BuiltNoteVisual _buildNoteVisual(
  VocalNote shown,
  VocalNote rawNote,
  EditorLayout layout,
  double viewStart,
  double pitchHi,
  bool bypass,
) {
  final count = _samplesFor(shown);
  final x0 = layout.timeToX(viewStart, shown.start);
  final x1 = math.max(x0 + 3.0, layout.timeToX(viewStart, shown.start + shown.dur));
  final centerY = layout.pitchToY(pitchHi, shown.pitch);

  final levelScale = clampVal(
    math.pow(10.0, shown.gainDb / 20.0).toDouble(),
    0.55,
    1.35,
  );
  final boxHalfH = clampVal(layout.pxPerSemi * 0.37 * levelScale, 6.0, 14.5);
  final radius = math.min(4.5, math.min(boxHalfH, (x1 - x0) * 0.35));
  final blockRRect = RRect.fromRectAndRadius(
    Rect.fromLTRB(x0, centerY - boxHalfH, x1, centerY + boxHalfH),
    Radius.circular(radius),
  );

  final pitchMoved = (shown.pitch - rawNote.detected).abs() > 0.045;
  final timeMoved = (shown.start - rawNote.detectedStart).abs() > 0.012 ||
      (shown.dur - rawNote.detectedDur).abs() > 0.012;
  final modChanged = (shown.modulation - 1.0).abs() > 0.05;
  final hasGhost = !bypass && !rawNote.unpitched && (pitchMoved || timeMoved || modChanged);

  final ghostCenterY = layout.pitchToY(pitchHi, rawNote.detected);
  RRect? ghostRRect;
  if (hasGhost && (pitchMoved || timeMoved)) {
    final gx0 = layout.timeToX(viewStart, rawNote.detectedStart);
    final gx1 = math.max(
      gx0 + 3.0,
      layout.timeToX(viewStart, rawNote.detectedStart + rawNote.detectedDur),
    );
    final gHalfH = clampVal(layout.pxPerSemi * 0.34, 5.5, 13.0);
    ghostRRect = RRect.fromRectAndRadius(
      Rect.fromLTRB(gx0, ghostCenterY - gHalfH, gx1, ghostCenterY + gHalfH),
      Radius.circular(math.min(4.0, gHalfH)),
    );
  }

  final top = <Offset>[];
  final bottom = <Offset>[];
  final curve = <Offset>[];
  final ghostCurve = <Offset>[];
  final unedited = presentNote(rawNote, true);

  for (int i = 0; i <= count; i += 1) {
    final u = i / count;
    final time = shown.start + shown.dur * u;
    final x = layout.timeToX(viewStart, time);
    final env = energy(shown, u);
    // Symmetric vocal waveform silhouette centered horizontally inside the note block
    final waveHalf = boxHalfH * (0.16 + 0.76 * env);
    top.add(Offset(x, centerY - waveHalf));
    bottom.add(Offset(x, centerY + waveHalf));

    final p = pitchAt(shown, u);
    curve.add(Offset(x, layout.pitchToY(pitchHi, p)));

    if (hasGhost) {
      final gt = unedited.start + unedited.dur * u;
      final gx = layout.timeToX(viewStart, gt);
      final gp = pitchAt(unedited, u);
      ghostCurve.add(Offset(gx, layout.pitchToY(pitchHi, gp)));
    }
  }

  final energyPath = Path();
  if (top.isNotEmpty) {
    _curveThrough(energyPath, top);
    _curveThrough(energyPath, bottom.reversed.toList(), move: false);
    energyPath.close();
  }

  return _BuiltNoteVisual(
    blockRRect: blockRRect,
    ghostRRect: ghostRRect,
    energyPath: energyPath,
    curve: curve,
    ghostCurve: ghostCurve,
    centerY: centerY,
    ghostCenterY: ghostCenterY,
    hasGhost: hasGhost,
  );
}

class EditorPainter extends CustomPainter {
  final List<VocalNote> notes;
  final String keyName;
  final String mode;
  final double bpm;
  final double duration;
  final double playhead;
  final bool loop;
  final bool bypass;
  final String? selectedId;
  final String? hoverId;
  final String tool;
  final double viewStart;
  final double viewDuration;
  final double pitchLo;
  final double pitchHi;
  final bool isDraggingPitch;
  final String? dragNoteId;

  EditorPainter({
    required this.notes,
    required this.keyName,
    required this.mode,
    required this.bpm,
    required this.duration,
    required this.playhead,
    required this.loop,
    required this.bypass,
    required this.selectedId,
    required this.hoverId,
    required this.tool,
    required this.viewStart,
    required this.viewDuration,
    required this.pitchLo,
    required this.pitchHi,
    required this.isDraggingPitch,
    required this.dragNoteId,
  });

  @override
  void paint(Canvas canvas, Size size) {
    if (size.width < 8 || size.height < 8) return;
    final layout = EditorLayout.compute(
      size.width,
      size.height,
      pitchLo: pitchLo,
      pitchHi: pitchHi,
      viewDuration: viewDuration,
    );

    _drawGrid(canvas, layout);

    canvas.save();
    canvas.clipRect(
      Rect.fromLTWH(
        layout.padL,
        layout.padT,
        layout.w - layout.padL,
        layout.h - layout.padT - layout.padB,
      ),
    );

    final timeOrdered = [...notes]..sort((a, b) => a.start.compareTo(b.start));
    final ordered = [...notes]..sort((a, b) {
        if (a.id == selectedId) return 1;
        if (b.id == selectedId) return -1;
        return a.start.compareTo(b.start);
      });

    final visuals = <String, _BuiltNoteVisual>{};
    for (final note in ordered) {
      final shown = presentNote(note, bypass);
      final x0 = layout.timeToX(viewStart, shown.start);
      final x1 = layout.timeToX(viewStart, shown.start + shown.dur);
      if (x1 < layout.padL - 30 || x0 > layout.w + 30) continue;
      visuals[note.id] = _buildNoteVisual(shown, note, layout, viewStart, pitchHi, bypass);
    }

    // 1. Draw ghost original note boxes and original pitch traces first
    for (final note in ordered) {
      final vis = visuals[note.id];
      if (vis == null || !vis.hasGhost) continue;
      _drawGhostNote(canvas, vis, note.id == selectedId);
    }

    // 2. Draw studio note blocks + inner symmetric vocal energy waveforms + in-note labels
    for (final note in ordered) {
      final vis = visuals[note.id];
      if (vis == null) continue;
      final shown = presentNote(note, bypass);
      final reading = centsFromTarget(shown.pitch, keyName, mode);
      var rgb = _rgbFor(note.unpitched ? 0 : reading.cents);
      final selected = note.id == selectedId;
      final hovered = note.id == hoverId && !selected;
      if (selected) {
        rgb = _lighten(rgb, 0.12);
      } else if (hovered) {
        rgb = _lighten(rgb, 0.06);
      }
      _drawNoteBlock(
        canvas,
        vis,
        shown,
        rgb,
        reading.cents,
        selected,
        note.unpitched,
        note.pinned,
      );
    }

    // 3. Draw smooth inter-note pitch transition threads
    _drawInterNoteTransitions(canvas, timeOrdered, visuals);

    // 4. Draw crisp active pitch contour splines on top of the note blocks
    for (final note in ordered) {
      final vis = visuals[note.id];
      if (vis == null) continue;
      _strokeCurve(canvas, vis.curve, note.id == selectedId, note.unpitched);
    }

    final selectedNote = notes.where((n) => n.id == selectedId).firstOrNull;
    _drawTargetGuide(canvas, layout, selectedNote);
    _drawHandles(canvas, layout, selectedNote);
    _drawDragGuide(canvas, layout);
    _drawSelectedBadge(canvas, layout, selectedNote, visuals[selectedId]);
    canvas.restore();

    _drawRulers(canvas, layout);
    _drawLyrics(canvas, layout);
    _drawLoop(canvas, layout);
    _drawPlayhead(canvas, layout);
    if (bypass) _drawBypassTag(canvas, layout);
  }

  void _drawGhostNote(
    Canvas canvas,
    _BuiltNoteVisual vis,
    bool selected,
  ) {
    final ghostRRect = vis.ghostRRect;
    if (ghostRRect != null) {
      final rect = ghostRRect.outerRect;
      // Connector stem between original pitch box and tuned pitch box
      final midX = (vis.blockRRect.left + vis.blockRRect.right) * 0.5;
      if ((vis.centerY - vis.ghostCenterY).abs() > 3.0) {
        canvas.drawLine(
          Offset(midX, vis.ghostCenterY),
          Offset(midX, vis.centerY),
          Paint()
            ..strokeWidth = 1.0
            ..color = selected
                ? const Color.fromRGBO(227, 154, 98, 0.36)
                : const Color.fromRGBO(255, 246, 232, 0.14),
        );
      }
      canvas.drawRRect(
        ghostRRect,
        Paint()
          ..style = PaintingStyle.fill
          ..color = selected
              ? const Color.fromRGBO(227, 154, 98, 0.09)
              : const Color.fromRGBO(255, 246, 232, 0.045),
      );
      canvas.drawRRect(
        ghostRRect,
        Paint()
          ..style = PaintingStyle.stroke
          ..strokeWidth = 1.0
          ..color = selected
              ? const Color.fromRGBO(227, 154, 98, 0.34)
              : const Color.fromRGBO(255, 246, 232, 0.16),
      );
      // Center line inside ghost box
      canvas.drawLine(
        Offset(rect.left + 3, vis.ghostCenterY),
        Offset(rect.right - 3, vis.ghostCenterY),
        Paint()
          ..strokeWidth = 1.0
          ..color = const Color.fromRGBO(255, 246, 232, 0.10),
      );
    }

    if (vis.ghostCurve.length >= 2) {
      final ghostPath = Path();
      _curveThrough(ghostPath, vis.ghostCurve);
      canvas.drawPath(
        ghostPath,
        Paint()
          ..style = PaintingStyle.stroke
          ..strokeJoin = StrokeJoin.round
          ..strokeCap = StrokeCap.round
          ..strokeWidth = 1.0
          ..color = selected
              ? const Color.fromRGBO(244, 239, 230, 0.28)
              : const Color.fromRGBO(244, 239, 230, 0.15),
      );
    }
  }

  void _drawNoteBlock(
    Canvas canvas,
    _BuiltNoteVisual vis,
    VocalNote shown,
    List<int> rgb,
    int cents,
    bool selected,
    bool unpitched,
    bool pinned,
  ) {
    final rrect = vis.blockRRect;
    final rect = rrect.outerRect;

    if (selected && !unpitched) {
      canvas.drawRRect(
        rrect.inflate(1.5),
        Paint()
          ..style = PaintingStyle.fill
          ..color = const Color.fromRGBO(227, 154, 98, 0.16),
      );
    }

    // 1. Outer Studio Capsule background
    final bgShader = ui.Gradient.linear(
      Offset(0, rect.top),
      Offset(0, rect.bottom),
      unpitched
          ? const [
              Color.fromRGBO(110, 106, 100, 0.36),
              Color.fromRGBO(82, 78, 74, 0.36),
            ]
          : [
              Color.fromRGBO(rgb[0], rgb[1], rgb[2], selected ? 0.46 : 0.34),
              Color.fromRGBO(
                (rgb[0] * 0.72).round(),
                (rgb[1] * 0.68).round(),
                (rgb[2] * 0.64).round(),
                selected ? 0.56 : 0.42,
              ),
            ],
    );
    canvas.drawRRect(
      rrect,
      Paint()
        ..style = PaintingStyle.fill
        ..shader = bgShader,
    );

    // 2. Clipped inner vocal energy silhouette + horizontal pitch center line
    canvas.save();
    canvas.clipRRect(rrect);

    // Pitch center reference line inside the block
    canvas.drawLine(
      Offset(rect.left, vis.centerY),
      Offset(rect.right, vis.centerY),
      Paint()
        ..strokeWidth = 1.0
        ..color = const Color.fromRGBO(255, 248, 238, 0.16),
    );

    // Symmetric inner vocal energy waveform blob
    final energyFill = Paint()
      ..style = PaintingStyle.fill
      ..shader = ui.Gradient.linear(
        Offset(0, rect.top),
        Offset(0, rect.bottom),
        unpitched
            ? const [
                Color.fromRGBO(146, 140, 132, 0.45),
                Color.fromRGBO(116, 110, 104, 0.45),
              ]
            : [
                Color.fromRGBO(
                  math.min(255, rgb[0] + 18),
                  math.min(255, rgb[1] + 16),
                  math.min(255, rgb[2] + 14),
                  selected ? 0.94 : 0.86,
                ),
                Color.fromRGBO(rgb[0], rgb[1], rgb[2], selected ? 0.96 : 0.88),
                Color.fromRGBO(
                  (rgb[0] * 0.78).round(),
                  (rgb[1] * 0.74).round(),
                  (rgb[2] * 0.70).round(),
                  selected ? 0.94 : 0.86,
                ),
              ],
        unpitched ? null : const [0.0, 0.5, 1.0],
      );
    canvas.drawPath(vis.energyPath, energyFill);

    // Subtle top studio specular sheen
    canvas.drawRect(
      Rect.fromLTWH(rect.left, rect.top, rect.width, rect.height * 0.38),
      Paint()
        ..shader = ui.Gradient.linear(
          Offset(0, rect.top),
          Offset(0, rect.top + rect.height * 0.38),
          [
            selected
                ? const Color.fromRGBO(255, 250, 242, 0.30)
                : const Color.fromRGBO(255, 246, 236, 0.18),
            const Color.fromRGBO(255, 246, 236, 0.0),
          ],
        ),
    );

    // In-block note name & tuning status (only when note block is wide enough)
    if (!unpitched && rect.width >= 26 && rect.height >= 12) {
      final noteName = spellMidi(shown.pitch, keyName);
      _drawText(
        canvas,
        text: noteName,
        style: CantoTypography.mono(
          size: 8.5,
          weight: FontWeight.w700,
          color: selected
              ? const Color.fromRGBO(28, 16, 10, 0.92)
              : const Color.fromRGBO(32, 18, 10, 0.82),
        ),
        position: Offset(rect.left + 5.0, vis.centerY),
        centerVertically: true,
      );

      if (rect.width >= 60 && !selected) {
        final absCents = cents.abs();
        final centsLabel = absCents <= 4
            ? '0¢'
            : '${cents > 0 ? '+' : '−'}$absCents¢';
        _drawText(
          canvas,
          text: centsLabel,
          style: CantoTypography.mono(
            size: 8.0,
            weight: FontWeight.w600,
            color: absCents <= 6
                ? const Color.fromRGBO(30, 18, 10, 0.68)
                : const Color.fromRGBO(42, 14, 10, 0.82),
          ),
          position: Offset(rect.right - 5.0, vis.centerY),
          alignRight: true,
          centerVertically: true,
        );
      }
    }

    canvas.restore();

    // 3. Precision capsule border
    final borderPaint = Paint()
      ..style = PaintingStyle.stroke
      ..strokeWidth = selected ? 1.5 : 1.0
      ..color = unpitched
          ? const Color.fromRGBO(255, 255, 255, 0.10)
          : selected
              ? const Color.fromRGBO(250, 245, 236, 0.96)
              : pinned
                  ? const Color.fromRGBO(255, 236, 210, 0.62)
                  : const Color.fromRGBO(44, 22, 10, 0.55);
    canvas.drawRRect(rrect, borderPaint);
  }

  void _drawInterNoteTransitions(
    Canvas canvas,
    List<VocalNote> timeOrdered,
    Map<String, _BuiltNoteVisual> visuals,
  ) {
    final threadPaint = Paint()
      ..style = PaintingStyle.stroke
      ..strokeWidth = 1.0
      ..strokeCap = StrokeCap.round
      ..color = const Color.fromRGBO(244, 236, 224, 0.28);

    for (int i = 0; i < timeOrdered.length - 1; i++) {
      final a = timeOrdered[i];
      final b = timeOrdered[i + 1];
      if (a.unpitched || b.unpitched) continue;
      final sa = presentNote(a, bypass);
      final sb = presentNote(b, bypass);
      final gap = sb.start - (sa.start + sa.dur);
      if (gap < -0.02 || gap > 0.16) continue;
      final va = visuals[a.id];
      final vb = visuals[b.id];
      if (va == null || vb == null || va.curve.isEmpty || vb.curve.isEmpty) {
        continue;
      }
      final p0 = va.curve.last;
      final p1 = vb.curve.first;
      final midX = (p0.dx + p1.dx) * 0.5;
      final path = Path()
        ..moveTo(p0.dx, p0.dy)
        ..cubicTo(midX, p0.dy, midX, p1.dy, p1.dx, p1.dy);
      canvas.drawPath(path, threadPaint);
    }
  }

  void _strokeCurve(
    Canvas canvas,
    List<Offset> curve,
    bool selected,
    bool unpitched,
  ) {
    if (unpitched || curve.length < 2) return;
    final path = Path();
    _curveThrough(path, curve);

    // Subtle dark halo so the pitch trace reads clearly over bright note fills and dark grid lines
    canvas.drawPath(
      path,
      Paint()
        ..style = PaintingStyle.stroke
        ..strokeJoin = StrokeJoin.round
        ..strokeCap = StrokeCap.round
        ..strokeWidth = selected ? 3.0 : 2.4
        ..color = const Color.fromRGBO(24, 14, 8, 0.52),
    );

    final paint = Paint()
      ..style = PaintingStyle.stroke
      ..strokeJoin = StrokeJoin.round
      ..strokeCap = StrokeCap.round
      ..strokeWidth = selected ? 1.65 : 1.25
      ..color = selected
          ? const Color.fromRGBO(255, 252, 246, 0.98)
          : const Color.fromRGBO(250, 242, 230, 0.86);
    canvas.drawPath(path, paint);
  }

  void _drawGrid(Canvas canvas, EditorLayout layout) {
    final w = layout.w;
    final h = layout.h;
    final padL = layout.padL;
    final padT = layout.padT;
    final padB = layout.padB;
    final gridBottom = h - padB;

    canvas.drawRect(
      Rect.fromLTWH(0, 0, w, h),
      Paint()..color = CantoColors.well,
    );
    final chromePaint = Paint()..color = CantoColors.panel;
    canvas.drawRect(Rect.fromLTWH(0, 0, w, padT), chromePaint);
    canvas.drawRect(Rect.fromLTWH(0, gridBottom, w, padB), chromePaint);
    canvas.drawRect(Rect.fromLTWH(0, 0, padL, h), chromePaint);

    final pcs = scalePcs(keyName, mode).toSet();
    final tonic = keyPc(keyName);

    canvas.save();
    canvas.clipRect(Rect.fromLTWH(padL, padT, w - padL, gridBottom - padT));

    final first = pitchLo.floor();
    final last = pitchHi.ceil();
    for (int pitch = first; pitch <= last; pitch += 1) {
      final pc = ((pitch % 12) + 12) % 12;
      if (!pcs.contains(pc)) continue;
      final y1 = layout.pitchToY(pitchHi, pitch + 0.5);
      final y2 = layout.pitchToY(pitchHi, pitch - 0.5);
      final rowPaint = Paint()
        ..color = pc == tonic
            ? const Color.fromRGBO(227, 154, 98, 0.06)
            : const Color.fromRGBO(255, 246, 232, 0.03);
      canvas.drawRect(Rect.fromLTWH(padL, y1, w - padL, y2 - y1), rowPaint);
    }

    final beat = 60.0 / bpm;
    final startBeat = (viewStart / beat).floor() - 1;
    final endBeat = ((viewStart + viewDuration) / beat).ceil() + 1;
    final beatWidth = beat * layout.pxPerSec;

    for (int beatIndex = startBeat; beatIndex <= endBeat; beatIndex += 1) {
      final x = layout.timeToX(viewStart, beatIndex * beat);
      if (x < padL - 2 || x > w + 2) continue;
      final barLine = beatIndex % 4 == 0;
      final linePaint = Paint()
        ..strokeWidth = 1
        ..color = barLine
            ? const Color.fromRGBO(255, 255, 255, 0.09)
            : const Color.fromRGBO(255, 255, 255, 0.035);
      final cx = x.roundToDouble() + 0.5;
      canvas.drawLine(Offset(cx, padT), Offset(cx, gridBottom), linePaint);

      if (beatWidth > 46 && !barLine) {
        final mid = layout.timeToX(viewStart, (beatIndex + 0.5) * beat);
        final midX = mid.roundToDouble() + 0.5;
        canvas.drawLine(
          Offset(midX, padT),
          Offset(midX, gridBottom),
          Paint()
            ..strokeWidth = 1
            ..color = const Color.fromRGBO(255, 255, 255, 0.025),
        );
      }
    }

    for (int pitch = first; pitch <= last; pitch += 1) {
      final y = layout.pitchToY(pitchHi, pitch.toDouble());
      final pc = ((pitch % 12) + 12) % 12;
      final isTonic = pc == tonic;
      final isC = pc == 0;
      final lineColor = isTonic
          ? const Color.fromRGBO(227, 154, 98, 0.32)
          : isC
              ? const Color.fromRGBO(255, 255, 255, 0.12)
              : const Color.fromRGBO(255, 255, 255, 0.045);
      final cy = y.roundToDouble() + 0.5;
      canvas.drawLine(
        Offset(padL, cy),
        Offset(w, cy),
        Paint()
          ..strokeWidth = 1
          ..color = lineColor,
      );
    }

    canvas.restore();

    final borderPaint = Paint()
      ..strokeWidth = 1
      ..color = const Color.fromRGBO(255, 255, 255, 0.06);
    canvas.drawLine(Offset(padL + 0.5, padT), Offset(padL + 0.5, h), borderPaint);
    canvas.drawLine(Offset(padL, padT + 0.5), Offset(w, padT + 0.5), borderPaint);
    canvas.drawLine(
      Offset(0, gridBottom + 0.5),
      Offset(w, gridBottom + 0.5),
      borderPaint,
    );
  }

  void _drawRulers(Canvas canvas, EditorLayout layout) {
    final w = layout.w;
    final h = layout.h;
    final padL = layout.padL;
    final padT = layout.padT;
    final padB = layout.padB;
    final gridBottom = h - padB;
    final pcs = scalePcs(keyName, mode).toSet();
    final tonic = keyPc(keyName);

    final first = pitchLo.ceil();
    final last = pitchHi.floor();
    for (int pitch = first; pitch <= last; pitch += 1) {
      final y = layout.pitchToY(pitchHi, pitch.toDouble());
      if (y < padT + 6 || y > gridBottom - 6) continue;
      final pc = ((pitch % 12) + 12) % 12;
      final inScale = pcs.contains(pc);
      final isC = pc == 0;
      if (layout.pxPerSemi < 15 && !inScale && !isC) continue;
      final color = pc == tonic
          ? CantoColors.accent
          : inScale
              ? const Color(0xFFC9C3B8)
              : CantoColors.canvasFaint;
      _drawText(
        canvas,
        text: spellMidi(pitch, keyName),
        style: CantoTypography.mono(
          size: 9,
          weight: FontWeight.w500,
          color: color,
        ),
        position: Offset(padL - 5, y),
        alignRight: true,
        centerVertically: true,
      );
    }

    final beat = 60.0 / bpm;
    final startBeat = (viewStart / beat).floor();
    final endBeat = ((viewStart + viewDuration) / beat).ceil();
    for (int beatIndex = startBeat; beatIndex <= endBeat; beatIndex += 1) {
      if (beatIndex % 4 != 0) continue;
      final x = layout.timeToX(viewStart, beatIndex * beat);
      if (x < padL || x > w - 12) continue;
      final bar = (beatIndex ~/ 4) + 1;
      _drawText(
        canvas,
        text: '$bar',
        style: CantoTypography.mono(
          size: 9,
          weight: FontWeight.w500,
          color: CantoColors.canvasMuted,
        ),
        position: Offset(x.roundToDouble() + 4, padT / 2.0),
        centerVertically: true,
      );
    }
  }

  void _drawLyrics(Canvas canvas, EditorLayout layout) {
    final h = layout.h;
    final padL = layout.padL;
    final padB = layout.padB;
    final baseline = h - padB / 2.0;
    double lastRight = padL;
    final ordered = [...notes]..sort((a, b) => a.start.compareTo(b.start));

    for (final note in ordered) {
      if (note.lyric.isEmpty) continue;
      final shown = presentNote(note, bypass);
      final x0 = layout.timeToX(viewStart, shown.start);
      final x1 = layout.timeToX(viewStart, shown.start + shown.dur);
      if (x1 < padL || x0 > layout.w) continue;
      final visible = math.min(x1, layout.w - 4) - math.max(x0, padL);
      if (visible < math.min(x1 - x0, 18.0) * 0.72) continue;
      final center = (math.max(x0, padL) + math.min(x1, layout.w)) / 2.0;
      if (x1 - x0 < 16) continue;
      final selected = note.id == selectedId;
      final tp = TextPainter(
        text: TextSpan(
          text: note.lyric,
          style: CantoTypography.sans(
            size: 10.5,
            weight: selected ? FontWeight.w600 : FontWeight.w500,
            color: selected ? CantoColors.ink : CantoColors.canvasMuted,
          ),
        ),
        textDirection: TextDirection.ltr,
      )..layout();
      if (center - tp.width / 2.0 < lastRight + 3) continue;
      tp.paint(
        canvas,
        Offset(center - tp.width / 2.0, baseline - tp.height / 2.0),
      );
      lastRight = center + tp.width / 2.0;
    }
  }

  void _drawSelectedBadge(
    Canvas canvas,
    EditorLayout layout,
    VocalNote? selectedNote,
    _BuiltNoteVisual? vis,
  ) {
    if (selectedNote == null || selectedNote.unpitched || vis == null) return;
    final shown = presentNote(selectedNote, bypass);
    final reading = centsFromTarget(shown.pitch, keyName, mode);
    final rect = vis.blockRRect.outerRect;
    if (rect.right < layout.padL || rect.left > layout.w) return;

    final noteText = spellMidi(shown.pitch, keyName);
    final centsText = formatCents(reading.cents);
    final badgeText = '$noteText · $centsText';

    final tp = TextPainter(
      text: TextSpan(
        text: badgeText,
        style: CantoTypography.mono(
          size: 9.0,
          weight: FontWeight.w600,
          color: reading.cents.abs() <= 6
              ? const Color(0xFFF5EFE6)
              : CantoColors.heat,
        ),
      ),
      textDirection: TextDirection.ltr,
    )..layout();

    final pillW = tp.width + 10.0;
    final pillH = tp.height + 4.0;
    final centerX = clampVal(
      rect.center.dx,
      layout.padL + pillW / 2.0 + 4.0,
      layout.w - pillW / 2.0 - 4.0,
    );
    double topY = rect.top - pillH - 5.0;
    if (topY < layout.padT + 3.0) {
      topY = math.min(layout.h - layout.padB - pillH - 3.0, rect.bottom + 5.0);
    }

    final pillRRect = RRect.fromRectAndRadius(
      Rect.fromLTWH(centerX - pillW / 2.0, topY, pillW, pillH),
      const Radius.circular(3.5),
    );
    canvas.drawRRect(
      pillRRect,
      Paint()..color = const Color.fromRGBO(22, 18, 15, 0.90),
    );
    canvas.drawRRect(
      pillRRect,
      Paint()
        ..style = PaintingStyle.stroke
        ..strokeWidth = 1.0
        ..color = reading.cents.abs() <= 6
            ? const Color.fromRGBO(227, 154, 98, 0.55)
            : const Color.fromRGBO(224, 112, 92, 0.65),
    );
    tp.paint(
      canvas,
      Offset(centerX - tp.width / 2.0, topY + (pillH - tp.height) / 2.0),
    );
  }

  void _drawPlayhead(Canvas canvas, EditorLayout layout) {
    final x = layout.timeToX(viewStart, playhead);
    if (x < layout.padL - 1 || x > layout.w + 1) return;
    final crisp = x.roundToDouble() + 0.5;
    final linePaint = Paint()
      ..color = CantoColors.playhead
      ..strokeWidth = 1;
    canvas.drawLine(Offset(crisp, 2), Offset(crisp, layout.h - 1), linePaint);

    final headPath = Path()
      ..moveTo(crisp - 4, layout.padT)
      ..lineTo(crisp + 4, layout.padT)
      ..lineTo(crisp, layout.padT + 6)
      ..close();
    canvas.drawPath(headPath, Paint()..color = CantoColors.playhead);
  }

  void _drawTargetGuide(
    Canvas canvas,
    EditorLayout layout,
    VocalNote? note,
  ) {
    if (note == null || note.unpitched) return;
    final shown = presentNote(note, bypass);
    final reading = centsFromTarget(shown.pitch, keyName, mode);
    if (reading.cents.abs() < 6) return;
    final x0 = layout.timeToX(viewStart, shown.start);
    final x1 = layout.timeToX(viewStart, shown.start + shown.dur);
    final y = layout.pitchToY(pitchHi, reading.target.toDouble()).roundToDouble() + 0.5;
    final paint = Paint()
      ..color = const Color.fromRGBO(227, 154, 98, 0.55)
      ..strokeWidth = 1;
    double cx = x0;
    while (cx < x1) {
      final nx = math.min(cx + 2, x1);
      canvas.drawLine(Offset(cx, y), Offset(nx, y), paint);
      cx += 5;
    }
  }

  void _drawDragGuide(Canvas canvas, EditorLayout layout) {
    if (!isDraggingPitch || dragNoteId == null) return;
    final note = notes.where((n) => n.id == dragNoteId).firstOrNull;
    if (note == null) return;
    final shown = presentNote(note, bypass);
    final y = layout.pitchToY(pitchHi, shown.pitch).roundToDouble() + 0.5;
    canvas.drawLine(
      Offset(layout.padL, y),
      Offset(layout.w, y),
      Paint()
        ..color = const Color.fromRGBO(236, 231, 222, 0.28)
        ..strokeWidth = 1,
    );
  }

  void _drawHandles(Canvas canvas, EditorLayout layout, VocalNote? note) {
    if (tool != 'time' || note == null || note.unpitched) return;
    final shown = presentNote(note, bypass);
    final y = layout.pitchToY(pitchHi, shown.pitch);
    for (final time in [shown.start, shown.start + shown.dur]) {
      final x = layout.timeToX(viewStart, time);
      final rect = Rect.fromLTWH(
        x.roundToDouble() - 3,
        y.roundToDouble() - 5,
        6,
        10,
      );
      canvas.drawRRect(
        RRect.fromRectAndRadius(rect, const Radius.circular(1.5)),
        Paint()..color = const Color(0xFFF3EEE6),
      );
      canvas.drawRRect(
        RRect.fromRectAndRadius(rect, const Radius.circular(1.5)),
        Paint()
          ..style = PaintingStyle.stroke
          ..strokeWidth = 1
          ..color = const Color(0xFF1A1612),
      );
    }
  }

  void _drawBypassTag(Canvas canvas, EditorLayout layout) {
    final tp = TextPainter(
      text: TextSpan(
        text: 'BYPASS',
        style: CantoTypography.sans(
          size: 9,
          weight: FontWeight.w600,
          color: const Color(0xFFF0B8A4),
          letterSpacing: 0.5,
        ),
      ),
      textDirection: TextDirection.ltr,
    )..layout();
    final width = tp.width + 12;
    final x = layout.w - width - 8;
    final y = layout.padT + 8;
    canvas.drawRect(
      Rect.fromLTWH(x, y, width, 16),
      Paint()..color = const Color.fromRGBO(58, 28, 24, 0.92),
    );
    tp.paint(canvas, Offset(x + 6, y + (16 - tp.height) / 2.0));
  }

  void _drawLoop(Canvas canvas, EditorLayout layout) {
    if (!loop || duration <= 0) return;
    final dur = duration;
    final x0 = math.max(layout.padL, layout.timeToX(viewStart, 0));
    final x1 = math.min(layout.w, layout.timeToX(viewStart, dur));
    if (x1 <= x0) return;
    canvas.drawRect(
      Rect.fromLTWH(x0, 0, x1 - x0, 2),
      Paint()..color = const Color.fromRGBO(227, 154, 98, 0.9),
    );
  }

  void _drawText(
    Canvas canvas, {
    required String text,
    required TextStyle style,
    required Offset position,
    bool alignRight = false,
    bool centerHorizontally = false,
    bool centerVertically = false,
    bool alignBottom = false,
  }) {
    final tp = TextPainter(
      text: TextSpan(text: text, style: style),
      textDirection: TextDirection.ltr,
    )..layout();
    double dx = position.dx;
    double dy = position.dy;
    if (alignRight) dx -= tp.width;
    if (centerHorizontally) dx -= tp.width / 2.0;
    if (centerVertically) dy -= tp.height / 2.0;
    if (alignBottom) dy -= tp.height;
    tp.paint(canvas, Offset(dx, dy));
  }

  @override
  bool shouldRepaint(covariant EditorPainter oldDelegate) => true;
}

class OverviewPainter extends CustomPainter {
  final List<VocalNote> notes;
  final List<double> waveformPeaks;
  final String keyName;
  final String mode;
  final double bpm;
  final double duration;
  final double playhead;
  final bool bypass;
  final String? selectedId;
  final double viewStart;
  final double viewDuration;
  final double pitchLo;
  final double pitchHi;

  OverviewPainter({
    required this.notes,
    this.waveformPeaks = const [],
    required this.keyName,
    required this.mode,
    required this.bpm,
    required this.duration,
    required this.playhead,
    required this.bypass,
    required this.selectedId,
    required this.viewStart,
    required this.viewDuration,
    required this.pitchLo,
    required this.pitchHi,
  });

  @override
  void paint(Canvas canvas, Size size) {
    final width = size.width;
    final height = size.height;
    if (width < 8 || height < 8) return;

    canvas.drawRect(
      Rect.fromLTWH(0, 0, width, height),
      Paint()..color = CantoColors.transportBg,
    );

    final dur = math.max(0.5, duration);
    double xOf(double time) => (time / dur) * width;
    final beat = beatsToSec(1, bpm);
    final totalBeats = (dur / beat).ceil();
    final step = math.max(1, (totalBeats / 32).ceil());

    for (int index = 0; index <= totalBeats; index += step) {
      final x = xOf(index * beat).roundToDouble() + 0.5;
      final alpha = index % 4 == 0 ? 0.05 : 0.022;
      canvas.drawLine(
        Offset(x, 0),
        Offset(x, height),
        Paint()
          ..strokeWidth = 1
          ..color = Color.fromRGBO(255, 255, 255, alpha),
      );
    }

    if (waveformPeaks.isNotEmpty) {
      final midY = height / 2.0;
      final maxHalf = (height - 8) * 0.45;
      final wavePaint = Paint()
        ..strokeWidth = math.max(1.0, width / waveformPeaks.length * 0.65)
        ..color = const Color.fromRGBO(255, 246, 232, 0.11);
      for (int i = 0; i < waveformPeaks.length; i++) {
        final x = ((i + 0.5) / waveformPeaks.length) * width;
        final h = math.max(1.0, waveformPeaks[i] * maxHalf);
        canvas.drawLine(Offset(x, midY - h), Offset(x, midY + h), wavePaint);
      }
    }

    final span = math.max(1.0, pitchHi - pitchLo);
    for (final note in notes) {
      final shown = presentNote(note, bypass);
      final x = xOf(shown.start);
      final w = math.max(1.5, xOf(shown.dur));
      final y = ((pitchHi - shown.pitch) / span) * (height - 12) + 6;
      final reading = centsFromTarget(shown.pitch, keyName, mode);
      final rgb = note.unpitched ? [120, 116, 110] : _rgbFor(reading.cents);
      final selected = note.id == selectedId;
      final opacity = note.unpitched ? 0.45 : (selected ? 1.0 : 0.9);
      final radius = math.min(3.0, w / 2.0);
      canvas.drawRRect(
        RRect.fromRectAndRadius(
          Rect.fromLTWH(x, y - 3, w, 6),
          Radius.circular(radius),
        ),
        Paint()..color = Color.fromRGBO(rgb[0], rgb[1], rgb[2], opacity),
      );
    }

    final vx = xOf(viewStart);
    final vw = math.max(8.0, xOf(viewDuration));
    canvas.drawRect(
      Rect.fromLTWH(vx, 0, vw, height),
      Paint()..color = const Color.fromRGBO(255, 255, 255, 0.04),
    );
    canvas.drawRect(
      Rect.fromLTWH(vx + 0.5, 0.5, math.max(1.0, vw - 1), height - 1),
      Paint()
        ..style = PaintingStyle.stroke
        ..strokeWidth = 1
        ..color = const Color.fromRGBO(244, 239, 230, 0.72),
    );

    final play = xOf(playhead).roundToDouble() + 0.5;
    canvas.drawLine(
      Offset(play, 0),
      Offset(play, height),
      Paint()
        ..strokeWidth = 1
        ..color = CantoColors.playhead,
    );
  }

  @override
  bool shouldRepaint(covariant OverviewPainter oldDelegate) => true;
}
