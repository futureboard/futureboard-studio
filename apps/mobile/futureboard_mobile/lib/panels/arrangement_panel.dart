// Futureboard Studio Arrangement Timeline Panel: Ruler, Track Headers, Clips & Waveforms.

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../engine/studio_controller.dart';
import '../models/studio_models.dart';
import '../theme/fb_tokens.dart';
import '../widgets/fb_controls.dart';

class ArrangementPanel extends StatefulWidget {
  final StudioController controller;

  const ArrangementPanel({super.key, required this.controller});

  @override
  State<ArrangementPanel> createState() => _ArrangementPanelState();
}

class _ArrangementPanelState extends State<ArrangementPanel> {
  final ScrollController _horizontalScroll = ScrollController();

  @override
  void dispose() {
    _horizontalScroll.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final c = widget.controller.colors;
    final p = widget.controller.project;
    final tel = widget.controller.telemetry;
    final pxPerBeat = widget.controller.timelineZoomPxPerBeat;
    const totalBeats = 32.0;
    const laneHeight = 68.0;
    const rulerHeight = 26.0;
    final timelineWidth = totalBeats * pxPerBeat;

    return Container(
      color: c.bgCanvas,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          // ── Arrangement Toolbar ──────────────────────────────────────────
          Container(
            height: 32,
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
                  Icon(LucideIcons.layoutList, size: 13, color: c.accentPrimary),
                  const SizedBox(width: 6),
                  Text('ARRANGEMENT', style: FbTypography.sectionHeader(c)),
                  const SizedBox(width: FbSpace.s3),
                  FbButton(
                    label: '+ Track',
                    icon: LucideIcons.plus,
                    compact: true,
                    colors: c,
                    onTap: () => widget.controller.addTrack(
                      'Synth ${p.tracks.length + 1}',
                      TrackKind.instrument,
                    ),
                  ),
                  const SizedBox(width: 4),
                  FbButton(
                    label: '+ MIDI Clip',
                    icon: LucideIcons.music,
                    compact: true,
                    colors: c,
                    onTap: () => widget.controller.addMidiClip(
                      p.selectedTrackId,
                      tel.playheadBeats.floorToDouble(),
                    ),
                  ),
                  const SizedBox(width: 4),
                  FbButton(
                    label: 'Import Audio',
                    icon: LucideIcons.fileAudio,
                    compact: true,
                    colors: c,
                    onTap: () => widget.controller.pickAndImportAudioFile(
                      p.selectedTrackId,
                      tel.playheadBeats.floorToDouble(),
                    ),
                  ),
                  const SizedBox(width: 4),
                  FbButton(
                    label: 'Split @ Playhead',
                    icon: LucideIcons.scissors,
                    compact: true,
                    colors: c,
                    onTap: widget.controller.splitSelectedClipAtPlayhead,
                  ),
                  const SizedBox(width: 4),
                  FbButton(
                    label: 'Delete Clip',
                    icon: LucideIcons.trash2,
                    compact: true,
                    colors: c,
                    onTap: widget.controller.deleteSelectedClip,
                  ),
                  const SizedBox(width: FbSpace.s4),
                  Text('ZOOM', style: FbTypography.micro(c)),
                  const SizedBox(width: 4),
                  FbIconButton(
                    icon: LucideIcons.zoomOut,
                    tooltip: 'Zoom Out Timeline',
                    size: 22,
                    colors: c,
                    onTap: () =>
                        widget.controller.setTimelineZoom(pxPerBeat - 6.0),
                  ),
                  FbIconButton(
                    icon: LucideIcons.zoomIn,
                    tooltip: 'Zoom In Timeline',
                    size: 22,
                    colors: c,
                    onTap: () =>
                        widget.controller.setTimelineZoom(pxPerBeat + 6.0),
                  ),
                ],
              ),
            ),
          ),

          // ── Main Split: Fixed Track Headers Left + Scrollable Timeline Right
          Expanded(
            child: SingleChildScrollView(
              child: Row(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  // Left Column: Track Header Corner + Track Headers
                  SizedBox(
                    width: FbSize.trackHeaderW,
                    child: Column(
                      children: [
                        Container(
                          height: rulerHeight,
                          padding: const EdgeInsets.symmetric(
                              horizontal: FbSpace.s3),
                          alignment: Alignment.centerLeft,
                          decoration: BoxDecoration(
                            color: c.bgPrimary,
                            border: Border(
                              right: BorderSide(color: c.borderDefault),
                              bottom: BorderSide(color: c.borderDefault),
                            ),
                          ),
                          child: Row(
                            children: [
                              Expanded(
                                child: Text(
                                  'TRACKS (${p.tracks.length})',
                                  maxLines: 1,
                                  overflow: TextOverflow.ellipsis,
                                  style: FbTypography.sectionHeader(c),
                                ),
                              ),
                              Text(
                                'M S R I',
                                style: FbTypography.monoReadout(
                                  c,
                                  size: 9.0,
                                  color: c.textMuted,
                                ),
                              ),
                            ],
                          ),
                        ),
                        for (final track in p.tracks)
                          _buildTrackHeader(track, laneHeight, c),
                      ],
                    ),
                  ),

                  // Right Area: Horizontally Scrollable Ruler + Track Lanes
                  Expanded(
                    child: SingleChildScrollView(
                      controller: _horizontalScroll,
                      scrollDirection: Axis.horizontal,
                      child: SizedBox(
                        width: timelineWidth,
                        child: Stack(
                          children: [
                            Column(
                              children: [
                                // Interactive Timeline Ruler (square corners)
                                GestureDetector(
                                  onTapDown: (d) {
                                    final beat =
                                        (d.localPosition.dx / pxPerBeat)
                                            .clamp(0.0, totalBeats);
                                    widget.controller.seekBeats(beat);
                                  },
                                  onHorizontalDragUpdate: (d) {
                                    final beat =
                                        (d.localPosition.dx / pxPerBeat)
                                            .clamp(0.0, totalBeats);
                                    widget.controller.seekBeats(beat);
                                  },
                                  child: CustomPaint(
                                    size: Size(timelineWidth, rulerHeight),
                                    painter: _TimelineRulerPainter(
                                      pxPerBeat: pxPerBeat,
                                      totalBeats: totalBeats,
                                      beatsPerBar: p.timeSigNum,
                                      loopEnabled: p.loopEnabled,
                                      loopStart: p.loopStartBeats,
                                      loopEnd: p.loopEndBeats,
                                      playheadBeats: tel.playheadBeats,
                                      colors: c,
                                    ),
                                  ),
                                ),

                                // Track Lanes
                                for (final track in p.tracks)
                                  _buildTrackLane(
                                    track,
                                    timelineWidth,
                                    laneHeight,
                                    pxPerBeat,
                                    p.timeSigNum,
                                    c,
                                  ),
                              ],
                            ),

                            // Playhead vertical line spanning ruler + all lanes
                            Positioned(
                              left: (tel.playheadBeats * pxPerBeat)
                                  .clamp(0.0, timelineWidth - 1),
                              top: 0,
                              bottom: 0,
                              child: IgnorePointer(
                                child: Container(
                                  width: 1.5,
                                  color: c.playhead,
                                ),
                              ),
                            ),
                          ],
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

  Widget _buildTrackHeader(TrackModel track, double height, FbColors c) {
    final selected = widget.controller.project.selectedTrackId == track.id;
    final trackColor = FbColors.parseHex(track.colorHex, c.accentPrimary);
    final meter = widget.controller.telemetry.meterForTrack(track.id);

    return GestureDetector(
      onTap: () => widget.controller.selectTrack(track.id),
      child: Container(
        height: height,
        decoration: BoxDecoration(
          color: selected ? c.bgTertiary : c.bgPrimary,
          border: Border(
            right: BorderSide(color: c.borderDefault, width: 1.0),
            bottom: BorderSide(color: c.borderSubtle, width: 1.0),
          ),
        ),
        child: Row(
          children: [
            // Color identification bar
            Container(width: 4, height: height, color: trackColor),
            Expanded(
              child: Padding(
                padding: const EdgeInsets.symmetric(
                    horizontal: FbSpace.s2 + 2, vertical: 5),
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  mainAxisAlignment: MainAxisAlignment.spaceBetween,
                  children: [
                    // Row 1: Track Name + Latched M / S / R / I buttons
                    Row(
                      children: [
                        Expanded(
                          child: Text(
                            track.name,
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                            style: TextStyle(
                              fontFamily: FbTypography.fontSans,
                              fontSize: 11.5,
                              fontWeight:
                                  selected ? FontWeight.w700 : FontWeight.w600,
                              color: c.textPrimary,
                            ),
                          ),
                        ),
                        FbLatchButton(
                          label: 'M',
                          tooltip: 'Mute Track',
                          active: track.muted,
                          latchColor: c.stateMute,
                          colors: c,
                          size: 19,
                          onTap: () =>
                              widget.controller.toggleTrackMute(track.id),
                        ),
                        const SizedBox(width: 2),
                        FbLatchButton(
                          label: 'S',
                          tooltip: 'Solo Track',
                          active: track.soloed,
                          latchColor: c.stateSolo,
                          colors: c,
                          size: 19,
                          onTap: () =>
                              widget.controller.toggleTrackSolo(track.id),
                        ),
                        const SizedBox(width: 2),
                        FbLatchButton(
                          label: 'R',
                          tooltip: 'Record Arm',
                          active: track.armed,
                          latchColor: c.stateArm,
                          colors: c,
                          size: 19,
                          onTap: () =>
                              widget.controller.toggleTrackArm(track.id),
                        ),
                        const SizedBox(width: 2),
                        FbLatchButton(
                          label: 'I',
                          tooltip: 'Input Monitor',
                          active: track.inputMonitoring,
                          latchColor: c.stateMonitor,
                          colors: c,
                          size: 19,
                          onTap: () =>
                              widget.controller.toggleTrackMonitor(track.id),
                        ),
                      ],
                    ),

                    // Row 2: Instrument / Insert quick chip + Volume slider + dB readout
                    Row(
                      children: [
                        InkWell(
                          onTap: () {
                            if (track.instrument != null) {
                              widget.controller.inspectPlugin(track.id, null);
                            } else if (track.inserts.isNotEmpty) {
                              widget.controller.inspectPlugin(track.id, 0);
                            }
                          },
                          borderRadius: BorderRadius.circular(FbRadius.xs),
                          child: Container(
                            padding: const EdgeInsets.symmetric(
                                horizontal: 5, vertical: 2),
                            decoration: BoxDecoration(
                              color: c.bgSunken,
                              borderRadius: BorderRadius.circular(FbRadius.xs),
                              border: Border.all(color: c.borderDefault),
                            ),
                            child: Text(
                              track.instrument?.name.split(' ').first ??
                                  (track.inserts.isNotEmpty
                                      ? track.inserts.first.name
                                          .split(' ')
                                          .first
                                      : track.kind.name.toUpperCase()),
                              style: FbTypography.monoReadout(
                                c,
                                size: 9.5,
                                color: trackColor,
                              ),
                            ),
                          ),
                        ),
                        const SizedBox(width: 6),
                        Expanded(
                          child: SliderTheme(
                            data: SliderTheme.of(context).copyWith(
                              trackHeight: 2.5,
                              thumbShape: const RoundSliderThumbShape(
                                  enabledThumbRadius: 5.0),
                              overlayShape: SliderComponentShape.noOverlay,
                              activeTrackColor: trackColor,
                              inactiveTrackColor: c.bgSunken,
                              thumbColor: c.textPrimary,
                            ),
                            child: Slider(
                              value: track.volumeDb.clamp(-48.0, 12.0),
                              min: -48.0,
                              max: 12.0,
                              onChanged: (v) => widget.controller
                                  .setTrackVolumeDb(track.id, v),
                            ),
                          ),
                        ),
                        const SizedBox(width: 4),
                        SizedBox(
                          width: 36,
                          child: Text(
                            '${track.volumeDb.toStringAsFixed(1)}dB',
                            textAlign: TextAlign.right,
                            style: FbTypography.monoReadout(
                              c,
                              size: 9.5,
                              color: c.textSecondary,
                            ),
                          ),
                        ),
                      ],
                    ),
                  ],
                ),
              ),
            ),
            // Mini vertical track meter on right edge of header
            Padding(
              padding: const EdgeInsets.symmetric(vertical: 4, horizontal: 2),
              child: FbStereoMeter(
                peakLDb: meter.peakLDb,
                peakRDb: meter.peakRDb,
                gainReductionDb: meter.gainReductionDb,
                colors: c,
                width: 8,
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _buildTrackLane(
    TrackModel track,
    double width,
    double height,
    double pxPerBeat,
    int beatsPerBar,
    FbColors c,
  ) {
    final selectedTrack =
        widget.controller.project.selectedTrackId == track.id;
    final trackColor = FbColors.parseHex(track.colorHex, c.accentPrimary);

    return GestureDetector(
      onTapDown: (d) {
        widget.controller.selectTrack(track.id);
      },
      onDoubleTapDown: (d) {
        final beat = (d.localPosition.dx / pxPerBeat).floorToDouble();
        if (track.kind == TrackKind.instrument ||
            track.kind == TrackKind.drumKit) {
          widget.controller.addMidiClip(track.id, beat);
        }
      },
      child: Container(
        width: width,
        height: height,
        decoration: BoxDecoration(
          color: selectedTrack
              ? c.bgSecondary.withValues(alpha: 0.55)
              : c.bgCanvas,
          border: Border(
            bottom: BorderSide(color: c.borderSubtle, width: 1.0),
          ),
        ),
        child: Stack(
          children: [
            // Square-cornered timeline bar & beat gridlines (`FbRadius.none`)
            CustomPaint(
              size: Size(width, height),
              painter: _LaneGridPainter(
                pxPerBeat: pxPerBeat,
                beatsPerBar: beatsPerBar,
                colors: c,
              ),
            ),

            // MIDI Clips
            for (final clip in track.midiClips)
              _buildMidiClipWidget(track, clip, height, pxPerBeat, trackColor, c),

            // Audio Clips
            for (final clip in track.audioClips)
              _buildAudioClipWidget(track, clip, height, pxPerBeat, trackColor, c),
          ],
        ),
      ),
    );
  }

  Widget _buildMidiClipWidget(
    TrackModel track,
    MidiClipModel clip,
    double laneHeight,
    double pxPerBeat,
    Color trackColor,
    FbColors c,
  ) {
    final isSelected = widget.controller.project.selectedClipId == clip.id;
    final left = clip.startBeat * pxPerBeat;
    final width = (clip.lengthBeats * pxPerBeat).clamp(16.0, 4096.0);

    return Positioned(
      left: left,
      top: 4,
      width: width,
      height: laneHeight - 8,
      child: GestureDetector(
        onTap: () {
          widget.controller.selectTrack(track.id, clipId: clip.id);
        },
        onDoubleTap: () {
          widget.controller.selectTrack(track.id, clipId: clip.id);
          widget.controller.setDockTab(StudioDockTab.pianoRoll);
        },
        onHorizontalDragUpdate: (d) {
          final deltaBeats = d.delta.dx / pxPerBeat;
          final nextStart =
              ((clip.startBeat + deltaBeats) * 4.0).round() / 4.0;
          widget.controller.updateClipBounds(
            track.id,
            clip.id,
            startBeat: nextStart.clamp(0.0, 64.0),
            lengthBeats: clip.lengthBeats,
          );
        },
        child: Container(
          decoration: BoxDecoration(
            color: trackColor.withValues(alpha: 0.22),
            borderRadius: BorderRadius.circular(FbRadius.sm),
            border: Border.all(
              color: isSelected ? c.textPrimary : trackColor,
              width: isSelected ? 1.6 : 1.0,
            ),
          ),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Container(
                height: 16,
                padding: const EdgeInsets.symmetric(horizontal: 5),
                decoration: BoxDecoration(
                  color: trackColor.withValues(alpha: 0.45),
                  borderRadius: const BorderRadius.vertical(
                    top: Radius.circular(FbRadius.xs),
                  ),
                ),
                alignment: Alignment.centerLeft,
                child: Text(
                  clip.name,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(
                    fontFamily: FbTypography.fontSans,
                    fontSize: 9.5,
                    fontWeight: FontWeight.w700,
                    color: c.textPrimary,
                  ),
                ),
              ),
              Expanded(
                child: CustomPaint(
                  painter: _MidiClipPreviewPainter(
                    notes: clip.notes,
                    lengthBeats: clip.lengthBeats,
                    color: trackColor,
                  ),
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }

  Widget _buildAudioClipWidget(
    TrackModel track,
    AudioClipModel clip,
    double laneHeight,
    double pxPerBeat,
    Color trackColor,
    FbColors c,
  ) {
    final isSelected = widget.controller.project.selectedClipId == clip.id;
    final left = clip.startBeat * pxPerBeat;
    final width = (clip.lengthBeats * pxPerBeat).clamp(16.0, 4096.0);

    return Positioned(
      left: left,
      top: 4,
      width: width,
      height: laneHeight - 8,
      child: GestureDetector(
        onTap: () {
          widget.controller.selectTrack(track.id, clipId: clip.id);
        },
        onDoubleTap: () {
          widget.controller.selectTrack(track.id, clipId: clip.id);
          widget.controller.setDockTab(StudioDockTab.pianoRoll);
        },
        onHorizontalDragUpdate: (d) {
          final deltaBeats = d.delta.dx / pxPerBeat;
          final nextStart =
              ((clip.startBeat + deltaBeats) * 4.0).round() / 4.0;
          widget.controller.updateClipBounds(
            track.id,
            clip.id,
            startBeat: nextStart.clamp(0.0, 64.0),
            lengthBeats: clip.lengthBeats,
          );
        },
        child: Container(
          decoration: BoxDecoration(
            color: trackColor.withValues(alpha: 0.20),
            borderRadius: BorderRadius.circular(FbRadius.sm),
            border: Border.all(
              color: isSelected ? c.textPrimary : trackColor,
              width: isSelected ? 1.6 : 1.0,
            ),
          ),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Container(
                height: 16,
                padding: const EdgeInsets.symmetric(horizontal: 5),
                decoration: BoxDecoration(
                  color: trackColor.withValues(alpha: 0.45),
                  borderRadius: const BorderRadius.vertical(
                    top: Radius.circular(FbRadius.xs),
                  ),
                ),
                alignment: Alignment.centerLeft,
                child: Text(
                  clip.name,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(
                    fontFamily: FbTypography.fontSans,
                    fontSize: 9.5,
                    fontWeight: FontWeight.w700,
                    color: c.textPrimary,
                  ),
                ),
              ),
              Expanded(
                child: CustomPaint(
                  painter: _AudioWaveformPainter(
                    peaks: clip.waveformPeaks,
                    color: trackColor,
                  ),
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

class _TimelineRulerPainter extends CustomPainter {
  final double pxPerBeat;
  final double totalBeats;
  final int beatsPerBar;
  final bool loopEnabled;
  final double loopStart;
  final double loopEnd;
  final double playheadBeats;
  final FbColors colors;

  _TimelineRulerPainter({
    required this.pxPerBeat,
    required this.totalBeats,
    required this.beatsPerBar,
    required this.loopEnabled,
    required this.loopStart,
    required this.loopEnd,
    required this.playheadBeats,
    required this.colors,
  });

  @override
  void paint(Canvas canvas, Size size) {
    canvas.drawRect(
      Offset.zero & size,
      Paint()..color = colors.bgPrimary,
    );

    if (loopEnabled && loopEnd > loopStart) {
      final lx0 = loopStart * pxPerBeat;
      final lx1 = loopEnd * pxPerBeat;
      canvas.drawRect(
        Rect.fromLTRB(lx0, 0, lx1, size.height),
        Paint()..color = colors.transportLoop.withValues(alpha: 0.16),
      );
      canvas.drawRect(
        Rect.fromLTRB(lx0, 0, lx1, 3),
        Paint()..color = colors.transportLoop,
      );
    }

    final bpb = beatsPerBar.clamp(1, 16);
    final totalInt = totalBeats.round();
    for (int beat = 0; beat <= totalInt; beat++) {
      final x = beat * pxPerBeat;
      final isBar = beat % bpb == 0;
      canvas.drawLine(
        Offset(x, isBar ? 0 : size.height * 0.55),
        Offset(x, size.height),
        Paint()
          ..color = isBar ? colors.borderStrong : colors.borderSubtle
          ..strokeWidth = 1.0,
      );
      if (isBar) {
        final barNum = (beat ~/ bpb) + 1;
        final tp = TextPainter(
          text: TextSpan(
            text: '$barNum',
            style: FbTypography.monoReadout(
              colors,
              size: 10.0,
              color: colors.textSecondary,
            ),
          ),
          textDirection: TextDirection.ltr,
        )..layout();
        tp.paint(canvas, Offset(x + 4, 5));
      }
    }

    // Playhead cap triangle
    final px = playheadBeats * pxPerBeat;
    final path = Path()
      ..moveTo(px - 5, 0)
      ..lineTo(px + 5, 0)
      ..lineTo(px, 8)
      ..close();
    canvas.drawPath(path, Paint()..color = colors.playhead);
  }

  @override
  bool shouldRepaint(covariant _TimelineRulerPainter oldDelegate) =>
      oldDelegate.playheadBeats != playheadBeats ||
      oldDelegate.pxPerBeat != pxPerBeat ||
      oldDelegate.loopEnabled != loopEnabled;
}

class _LaneGridPainter extends CustomPainter {
  final double pxPerBeat;
  final int beatsPerBar;
  final FbColors colors;

  _LaneGridPainter({
    required this.pxPerBeat,
    required this.beatsPerBar,
    required this.colors,
  });

  @override
  void paint(Canvas canvas, Size size) {
    final bpb = beatsPerBar.clamp(1, 16);
    final totalBeats = (size.width / pxPerBeat).ceil();
    for (int beat = 0; beat <= totalBeats; beat++) {
      final x = beat * pxPerBeat;
      final isBar = beat % bpb == 0;
      canvas.drawLine(
        Offset(x, 0),
        Offset(x, size.height),
        Paint()
          ..color = isBar ? colors.gridBarLine : colors.gridBeatLine
          ..strokeWidth = 1.0,
      );
    }
  }

  @override
  bool shouldRepaint(covariant _LaneGridPainter oldDelegate) =>
      oldDelegate.pxPerBeat != pxPerBeat;
}

class _MidiClipPreviewPainter extends CustomPainter {
  final List<MidiNoteModel> notes;
  final double lengthBeats;
  final Color color;

  _MidiClipPreviewPainter({
    required this.notes,
    required this.lengthBeats,
    required this.color,
  });

  @override
  void paint(Canvas canvas, Size size) {
    if (notes.isEmpty || lengthBeats <= 0) return;
    int minP = 127;
    int maxP = 0;
    for (final n in notes) {
      if (n.pitch < minP) minP = n.pitch;
      if (n.pitch > maxP) maxP = n.pitch;
    }
    final spanP = (maxP - minP + 4).clamp(8, 48);
    final notePaint = Paint()..color = color.withValues(alpha: 0.92);

    for (final n in notes) {
      final x = (n.startBeat / lengthBeats) * size.width;
      final w = ((n.durationBeats / lengthBeats) * size.width).clamp(2.5, size.width);
      final normY = 1.0 - ((n.pitch - minP + 2) / spanP).clamp(0.0, 1.0);
      final y = normY * (size.height - 5.0) + 1.0;
      canvas.drawRect(Rect.fromLTWH(x, y, w, 3.2), notePaint);
    }
  }

  @override
  bool shouldRepaint(covariant _MidiClipPreviewPainter oldDelegate) => true;
}

class _AudioWaveformPainter extends CustomPainter {
  final List<double> peaks;
  final Color color;

  _AudioWaveformPainter({required this.peaks, required this.color});

  @override
  void paint(Canvas canvas, Size size) {
    if (peaks.isEmpty) return;
    final midY = size.height / 2.0;
    final barW = size.width / peaks.length;
    final paint = Paint()
      ..color = color.withValues(alpha: 0.88)
      ..strokeWidth = (barW * 0.72).clamp(1.0, 3.0);

    for (int i = 0; i < peaks.length; i++) {
      final x = (i + 0.5) * barW;
      final amp = (peaks[i].clamp(0.04, 1.0)) * (size.height * 0.44);
      canvas.drawLine(Offset(x, midY - amp), Offset(x, midY + amp), paint);
    }
  }

  @override
  bool shouldRepaint(covariant _AudioWaveformPainter oldDelegate) =>
      oldDelegate.peaks != peaks;
}
