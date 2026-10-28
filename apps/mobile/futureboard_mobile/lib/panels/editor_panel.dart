// Futureboard Studio Piano Roll MIDI Editor & Audio Clip Waveform Editor.

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../engine/studio_controller.dart';
import '../models/studio_models.dart';
import '../theme/fb_tokens.dart';
import '../widgets/fb_controls.dart';

class EditorPanel extends StatefulWidget {
  final StudioController controller;

  const EditorPanel({super.key, required this.controller});

  @override
  State<EditorPanel> createState() => _EditorPanelState();
}

class _EditorPanelState extends State<EditorPanel> {
  static const List<String> _noteNames = [
    'C',
    'C#',
    'D',
    'D#',
    'E',
    'F',
    'F#',
    'G',
    'G#',
    'A',
    'A#',
    'B'
  ];

  bool _isBlackKey(int pitch) {
    final m = pitch % 12;
    return m == 1 || m == 3 || m == 6 || m == 8 || m == 10;
  }

  String _pitchLabel(int pitch, bool isDrum) {
    if (isDrum) {
      const drumLabels = {
        36: '36 Kick 808',
        37: '37 Kick Ac',
        38: '38 Snare',
        39: '39 Clap',
        40: '40 Rimshot',
        41: '41 Closed HH',
        42: '42 Pedal HH',
        43: '43 Open HH',
        44: '44 Tom Low',
        45: '45 Tom Mid',
        46: '46 Tom High',
        47: '47 Crash',
      };
      if (drumLabels.containsKey(pitch)) return drumLabels[pitch]!;
    }
    final octave = (pitch ~/ 12) - 1;
    return '${_noteNames[pitch % 12]}$octave';
  }

  @override
  Widget build(BuildContext context) {
    final c = widget.controller.colors;
    final track = widget.controller.selectedTrack;
    final midiClip = widget.controller.selectedMidiClip;
    final audioClip = widget.controller.selectedAudioClip;

    if (track.kind == TrackKind.audio && audioClip != null) {
      return _buildAudioClipEditor(track, audioClip, c);
    }

    if (midiClip == null) {
      return Container(
        color: c.bgCanvas,
        alignment: Alignment.center,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Text(
              'No MIDI Clip on ${track.name}',
              style: FbTypography.title(c),
            ),
            const SizedBox(height: 8),
            FbButton(
              label: '+ Create 4-Bar MIDI Clip',
              icon: LucideIcons.plus,
              colors: c,
              onTap: () => widget.controller.addMidiClip(track.id, 0.0),
            ),
          ],
        ),
      );
    }

    return _buildPianoRollEditor(track, midiClip, c);
  }

  Widget _buildPianoRollEditor(
    TrackModel track,
    MidiClipModel clip,
    FbColors c,
  ) {
    final isDrum = track.kind == TrackKind.drumKit;
    final minPitch = isDrum ? 36 : 48; // C2..C5 or Drum pads 36..51
    final maxPitch = isDrum ? 51 : 84;
    final pitchCount = maxPitch - minPitch + 1;
    const rowHeight = 20.0;
    final pxPerBeat = widget.controller.pianoRollZoomPxPerBeat;
    final totalBeats = clip.lengthBeats.clamp(4.0, 64.0);
    final gridWidth = totalBeats * pxPerBeat;
    final gridHeight = pitchCount * rowHeight;
    final trackColor = FbColors.parseHex(clip.colorHex, c.accentPrimary);

    return Container(
      color: c.bgCanvas,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          // Piano Roll Toolbar
          Container(
            height: 30,
            padding: const EdgeInsets.symmetric(horizontal: FbSpace.s3),
            decoration: BoxDecoration(
              color: c.bgSecondary,
              border: Border(
                bottom: BorderSide(color: c.borderDefault, width: 1.0),
              ),
            ),
            child: SingleChildScrollView(
              scrollDirection: Axis.horizontal,
              child: Row(
                children: [
                  Icon(LucideIcons.piano, size: 13, color: trackColor),
                  const SizedBox(width: 6),
                  Text(
                    'PIANO ROLL • ${track.name} / ${clip.name}',
                    style: FbTypography.sectionHeader(c),
                  ),
                  const SizedBox(width: FbSpace.s3),
                  FbButton(
                    label: 'Quantize 1/16',
                    icon: LucideIcons.grid2x2,
                    compact: true,
                    colors: c,
                    onTap: () =>
                        widget.controller.quantizeClip(track.id, clip.id, 0.25),
                  ),
                  const SizedBox(width: 4),
                  FbButton(
                    label: 'Quantize 1/8',
                    compact: true,
                    colors: c,
                    onTap: () =>
                        widget.controller.quantizeClip(track.id, clip.id, 0.5),
                  ),
                  const SizedBox(width: FbSpace.s3),
                  Text(
                    'Tap grid to draw note • Double-tap note to delete • Tap keys to audition',
                    style: FbTypography.micro(c),
                  ),
                ],
              ),
            ),
          ),

          // Scrollable Piano Roll Body
          Expanded(
            child: SingleChildScrollView(
              child: Row(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  // Left Keyboard / Drum Pad Gutter
                  SizedBox(
                    width: 82,
                    height: gridHeight,
                    child: Column(
                      children: [
                        for (int i = 0; i < pitchCount; i++)
                          _buildKeyGutterRow(
                            trackId: track.id,
                            pitch: maxPitch - i,
                            height: rowHeight,
                            isDrum: isDrum,
                            c: c,
                          ),
                      ],
                    ),
                  ),

                  // Right Note Grid Canvas
                  Expanded(
                    child: SingleChildScrollView(
                      scrollDirection: Axis.horizontal,
                      child: SizedBox(
                        width: gridWidth,
                        height: gridHeight,
                        child: GestureDetector(
                          onTapUp: (d) {
                            final row = (d.localPosition.dy ~/ rowHeight)
                                .clamp(0, pitchCount - 1);
                            final pitch = maxPitch - row;
                            final beat = ((d.localPosition.dx / pxPerBeat) * 4.0)
                                    .floor() /
                                4.0;
                            widget.controller.triggerLiveMidi(
                                pitch, 108, true,
                                trackId: track.id);
                            Future.delayed(const Duration(milliseconds: 160),
                                () {
                              widget.controller.triggerLiveMidi(
                                  pitch, 0, false,
                                  trackId: track.id);
                            });
                            widget.controller.upsertMidiNote(
                              trackId: track.id,
                              clipId: clip.id,
                              pitch: pitch,
                              startBeat: beat.clamp(0.0, totalBeats - 0.25),
                              durationBeats: 0.5,
                              velocity: 108,
                            );
                          },
                          child: Stack(
                            children: [
                              CustomPaint(
                                size: Size(gridWidth, gridHeight),
                                painter: _PianoRollGridPainter(
                                  minPitch: minPitch,
                                  maxPitch: maxPitch,
                                  rowHeight: rowHeight,
                                  pxPerBeat: pxPerBeat,
                                  colors: c,
                                ),
                              ),
                              for (final note in clip.notes)
                                if (note.pitch >= minPitch &&
                                    note.pitch <= maxPitch)
                                  _buildNoteBlock(
                                    track: track,
                                    clip: clip,
                                    note: note,
                                    maxPitch: maxPitch,
                                    rowHeight: rowHeight,
                                    pxPerBeat: pxPerBeat,
                                    color: trackColor,
                                    c: c,
                                  ),
                            ],
                          ),
                        ),
                      ),
                    ),
                  ),
                ],
              ),
            ),
          ),
        ],
      ),
    );
  }

  Widget _buildKeyGutterRow({
    required String trackId,
    required int pitch,
    required double height,
    required bool isDrum,
    required FbColors c,
  }) {
    final black = _isBlackKey(pitch);
    return GestureDetector(
      onTapDown: (_) =>
          widget.controller.triggerLiveMidi(pitch, 110, true, trackId: trackId),
      onTapUp: (_) =>
          widget.controller.triggerLiveMidi(pitch, 0, false, trackId: trackId),
      onTapCancel: () =>
          widget.controller.triggerLiveMidi(pitch, 0, false, trackId: trackId),
      child: Container(
        height: height,
        padding: const EdgeInsets.symmetric(horizontal: 6),
        alignment: Alignment.centerRight,
        decoration: BoxDecoration(
          color: black ? c.bgSunken : c.bgSecondary,
          border: Border(
            right: BorderSide(color: c.borderDefault),
            bottom: BorderSide(color: c.borderSubtle, width: 0.5),
          ),
        ),
        child: Text(
          _pitchLabel(pitch, isDrum),
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: FbTypography.monoReadout(
            c,
            size: 9.0,
            color: pitch % 12 == 0 ? c.accentPrimary : c.textSecondary,
          ),
        ),
      ),
    );
  }

  Widget _buildNoteBlock({
    required TrackModel track,
    required MidiClipModel clip,
    required MidiNoteModel note,
    required int maxPitch,
    required double rowHeight,
    required double pxPerBeat,
    required Color color,
    required FbColors c,
  }) {
    final top = (maxPitch - note.pitch) * rowHeight + 1.5;
    final left = note.startBeat * pxPerBeat;
    final width = (note.durationBeats * pxPerBeat).clamp(8.0, 2000.0);

    return Positioned(
      left: left,
      top: top,
      width: width,
      height: rowHeight - 3.0,
      child: GestureDetector(
        onDoubleTap: () => widget.controller.deleteMidiNote(
          trackId: track.id,
          clipId: clip.id,
          noteId: note.id,
        ),
        onLongPress: () => widget.controller.deleteMidiNote(
          trackId: track.id,
          clipId: clip.id,
          noteId: note.id,
        ),
        onHorizontalDragUpdate: (d) {
          final deltaBeats = d.delta.dx / pxPerBeat;
          final nextStart =
              ((note.startBeat + deltaBeats) * 4.0).round() / 4.0;
          widget.controller.upsertMidiNote(
            trackId: track.id,
            clipId: clip.id,
            noteId: note.id,
            pitch: note.pitch,
            startBeat: nextStart.clamp(0.0, clip.lengthBeats - 0.25),
            durationBeats: note.durationBeats,
            velocity: note.velocity,
          );
        },
        child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 3),
          alignment: Alignment.centerLeft,
          decoration: BoxDecoration(
            color: color.withValues(alpha: 0.85),
            borderRadius: BorderRadius.circular(2.0),
            border: Border.all(color: c.textPrimary.withValues(alpha: 0.7)),
          ),
          child: Text(
            _pitchLabel(note.pitch, false),
            maxLines: 1,
            overflow: TextOverflow.clip,
            style: TextStyle(
              fontFamily: FbTypography.fontMono,
              fontSize: 8.5,
              fontWeight: FontWeight.w700,
              color: c.textInverse,
            ),
          ),
        ),
      ),
    );
  }

  Widget _buildAudioClipEditor(
    TrackModel track,
    AudioClipModel clip,
    FbColors c,
  ) {
    final trackColor = FbColors.parseHex(clip.colorHex, c.accentPrimary);

    return Container(
      color: c.bgCanvas,
      padding: const EdgeInsets.all(FbSpace.s3),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(
            children: [
              Icon(LucideIcons.audioWaveform, size: 14, color: trackColor),
              const SizedBox(width: 6),
              Text(
                'AUDIO CLIP EDITOR • ${clip.name}',
                style: FbTypography.sectionHeader(c),
              ),
              const Spacer(),
              FbRotaryKnob(
                label: 'Clip Gain',
                value: clip.gainDb,
                min: -24.0,
                max: 18.0,
                defaultValue: 0.0,
                unit: 'dB',
                colors: c,
                accentColor: trackColor,
                diameter: 34,
                onChanged: (v) => widget.controller.updateClipBounds(
                  track.id,
                  clip.id,
                  startBeat: clip.startBeat,
                  lengthBeats: clip.lengthBeats,
                  gainDb: v,
                  pitchSemitones: clip.pitchSemitones,
                ),
              ),
              const SizedBox(width: FbSpace.s4),
              FbRotaryKnob(
                label: 'Transpose',
                value: clip.pitchSemitones,
                min: -12.0,
                max: 12.0,
                defaultValue: 0.0,
                unit: 'st',
                colors: c,
                accentColor: trackColor,
                diameter: 34,
                onChanged: (v) => widget.controller.updateClipBounds(
                  track.id,
                  clip.id,
                  startBeat: clip.startBeat,
                  lengthBeats: clip.lengthBeats,
                  gainDb: clip.gainDb,
                  pitchSemitones: v,
                ),
              ),
            ],
          ),
          const SizedBox(height: FbSpace.s2),
          Expanded(
            child: Container(
              decoration: BoxDecoration(
                color: c.bgSunken,
                border: Border.all(color: c.borderDefault),
              ),
              child: CustomPaint(
                painter: _HighResWaveformPainter(
                  peaks: clip.waveformPeaks,
                  color: trackColor,
                  colors: c,
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }
}

class _PianoRollGridPainter extends CustomPainter {
  final int minPitch;
  final int maxPitch;
  final double rowHeight;
  final double pxPerBeat;
  final FbColors colors;

  _PianoRollGridPainter({
    required this.minPitch,
    required this.maxPitch,
    required this.rowHeight,
    required this.pxPerBeat,
    required this.colors,
  });

  bool _isBlack(int p) {
    final m = p % 12;
    return m == 1 || m == 3 || m == 6 || m == 8 || m == 10;
  }

  @override
  void paint(Canvas canvas, Size size) {
    final count = maxPitch - minPitch + 1;
    for (int i = 0; i < count; i++) {
      final p = maxPitch - i;
      final y = i * rowHeight;
      canvas.drawRect(
        Rect.fromLTWH(0, y, size.width, rowHeight),
        Paint()
          ..color = _isBlack(p) ? colors.bgSunken : colors.bgCanvas,
      );
      canvas.drawLine(
        Offset(0, y + rowHeight),
        Offset(size.width, y + rowHeight),
        Paint()
          ..color = p % 12 == 0 ? colors.borderDefault : colors.gridSubLine
          ..strokeWidth = 1.0,
      );
    }

    final subDiv = pxPerBeat / 4.0;
    final totalSubs = (size.width / subDiv).ceil();
    for (int s = 0; s <= totalSubs; s++) {
      final x = s * subDiv;
      final isBar = s % 16 == 0;
      final isBeat = s % 4 == 0;
      canvas.drawLine(
        Offset(x, 0),
        Offset(x, size.height),
        Paint()
          ..color = isBar
              ? colors.gridBarLine
              : (isBeat ? colors.gridBeatLine : colors.gridSubLine)
          ..strokeWidth = 1.0,
      );
    }
  }

  @override
  bool shouldRepaint(covariant _PianoRollGridPainter oldDelegate) =>
      oldDelegate.pxPerBeat != pxPerBeat;
}

class _HighResWaveformPainter extends CustomPainter {
  final List<double> peaks;
  final Color color;
  final FbColors colors;

  _HighResWaveformPainter({
    required this.peaks,
    required this.color,
    required this.colors,
  });

  @override
  void paint(Canvas canvas, Size size) {
    final midY = size.height / 2.0;
    canvas.drawLine(
      Offset(0, midY),
      Offset(size.width, midY),
      Paint()..color = colors.borderDefault,
    );
    if (peaks.isEmpty) return;

    final barW = size.width / peaks.length;
    final paint = Paint()
      ..color = color
      ..strokeWidth = (barW * 0.75).clamp(1.2, 4.0);
    for (int i = 0; i < peaks.length; i++) {
      final x = (i + 0.5) * barW;
      final h = peaks[i].clamp(0.02, 1.0) * (size.height * 0.45);
      canvas.drawLine(Offset(x, midY - h), Offset(x, midY + h), paint);
    }
  }

  @override
  bool shouldRepaint(covariant _HighResWaveformPainter oldDelegate) => true;
}
