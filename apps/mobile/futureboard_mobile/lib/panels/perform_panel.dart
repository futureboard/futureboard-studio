// Futureboard Studio Perform Panel: Playable Multi-Octave Keyboard, 16 Drum Pads & Chord Strips.

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../engine/studio_controller.dart';
import '../theme/fb_tokens.dart';
import '../widgets/fb_controls.dart';

class PerformPanel extends StatefulWidget {
  final StudioController controller;

  const PerformPanel({super.key, required this.controller});

  @override
  State<PerformPanel> createState() => _PerformPanelState();
}

class _PerformPanelState extends State<PerformPanel> {
  final Set<int> _heldNotes = <int>{};

  void _noteDown(int note) {
    setState(() => _heldNotes.add(note));
    widget.controller.triggerLiveMidi(note, 114, true);
  }

  void _noteUp(int note) {
    setState(() => _heldNotes.remove(note));
    widget.controller.triggerLiveMidi(note, 0, false);
  }

  void _chordDown(List<int> notes) {
    for (final n in notes) {
      _noteDown(n);
    }
  }

  void _chordUp(List<int> notes) {
    for (final n in notes) {
      _noteUp(n);
    }
  }

  @override
  Widget build(BuildContext context) {
    final c = widget.controller.colors;
    final track = widget.controller.selectedTrack;
    final basePitch = (widget.controller.performOctave + 1) * 12; // e.g., C3 = 48

    const chords = <String, List<int>>{
      'Am9': [57, 60, 64, 67, 71],
      'Fmaj7': [53, 57, 60, 64],
      'Cmaj9': [60, 64, 67, 71, 74],
      'G6': [55, 59, 62, 64],
      'Dm7': [50, 53, 57, 60],
      'Em7': [52, 55, 59, 62],
    };

    return Container(
      color: c.bgCanvas,
      padding: const EdgeInsets.all(FbSpace.s3),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          // Top Perform Bar: Target Track + Octave Shift + Chord Trigger Pads
          SingleChildScrollView(
            scrollDirection: Axis.horizontal,
            child: Row(
              children: [
                Icon(LucideIcons.keyboardMusic,
                    size: 14, color: c.accentPrimary),
                const SizedBox(width: 6),
                Text(
                  'LIVE PERFORM • ${track.name.toUpperCase()}',
                  style: FbTypography.sectionHeader(c),
                ),
                const SizedBox(width: FbSpace.s3),
                FbButton(
                  label: 'Oct -',
                  compact: true,
                  colors: c,
                  onTap: () => setState(() {
                    widget.controller.performOctave =
                        (widget.controller.performOctave - 1).clamp(1, 6);
                  }),
                ),
                const SizedBox(width: 4),
                Text(
                  'C${widget.controller.performOctave}',
                  style: FbTypography.monoReadout(c, size: 11.5),
                ),
                const SizedBox(width: 4),
                FbButton(
                  label: 'Oct +',
                  compact: true,
                  colors: c,
                  onTap: () => setState(() {
                    widget.controller.performOctave =
                        (widget.controller.performOctave + 1).clamp(1, 6);
                  }),
                ),
                const SizedBox(width: FbSpace.s4),
                Text('CHORD PADS:', style: FbTypography.micro(c)),
                const SizedBox(width: 6),
                for (final entry in chords.entries)
                  Padding(
                    padding: const EdgeInsets.only(right: 4),
                    child: GestureDetector(
                      onTapDown: (_) => _chordDown(entry.value),
                      onTapUp: (_) => _chordUp(entry.value),
                      onTapCancel: () => _chordUp(entry.value),
                      child: Container(
                        padding: const EdgeInsets.symmetric(
                            horizontal: 9, vertical: 4),
                        decoration: BoxDecoration(
                          color: c.bgSecondary,
                          borderRadius: BorderRadius.circular(FbRadius.xs),
                          border: Border.all(color: c.accentPrimary),
                        ),
                        child: Text(
                          entry.key,
                          style: FbTypography.monoReadout(
                            c,
                            size: 10.5,
                            color: c.accentPrimary,
                          ),
                        ),
                      ),
                    ),
                  ),
              ],
            ),
          ),
          const SizedBox(height: FbSpace.s2),

          // Playable 24-Key Chromatic Piano Strip + 8 Quick Drum Trigger Pads
          Expanded(
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                // 8 Quick Drum Pads on Left
                SizedBox(
                  width: 190,
                  child: GridView.builder(
                    itemCount: 8,
                    gridDelegate:
                        const SliverGridDelegateWithFixedCrossAxisCount(
                      crossAxisCount: 2,
                      mainAxisSpacing: 4,
                      crossAxisSpacing: 4,
                      childAspectRatio: 1.9,
                    ),
                    itemBuilder: (context, idx) {
                      const labels = [
                        'Kick 808',
                        'Snare',
                        'Clap',
                        'Closed HH',
                        'Open HH',
                        'Tom Low',
                        'Crash',
                        'Cowbell'
                      ];
                      const notes = [36, 38, 39, 41, 43, 44, 47, 49];
                      final note = notes[idx];
                      final held = _heldNotes.contains(note);
                      return GestureDetector(
                        onTapDown: (_) => _noteDown(note),
                        onTapUp: (_) => _noteUp(note),
                        onTapCancel: () => _noteUp(note),
                        child: Container(
                          alignment: Alignment.center,
                          decoration: BoxDecoration(
                            color: held
                                ? c.stateMute.withValues(alpha: 0.35)
                                : c.bgSecondary,
                            borderRadius: BorderRadius.circular(FbRadius.sm),
                            border: Border.all(
                              color: held ? c.stateMute : c.borderDefault,
                            ),
                          ),
                          child: Text(
                            labels[idx],
                            style: FbTypography.body(c),
                          ),
                        ),
                      );
                    },
                  ),
                ),
                const SizedBox(width: FbSpace.s3),

                // 2-Octave Chromatic Keyboard
                Expanded(
                  child: Row(
                    crossAxisAlignment: CrossAxisAlignment.stretch,
                    children: [
                      for (int i = 0; i < 16; i++)
                        Expanded(
                          child: _buildPianoKey(basePitch + i, c),
                        ),
                    ],
                  ),
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }

  Widget _buildPianoKey(int pitch, FbColors c) {
    final m = pitch % 12;
    final isBlack = m == 1 || m == 3 || m == 6 || m == 8 || m == 10;
    final held = _heldNotes.contains(pitch);
    const names = [
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
    final label = '${names[m]}${(pitch ~/ 12) - 1}';

    return GestureDetector(
      onTapDown: (_) => _noteDown(pitch),
      onTapUp: (_) => _noteUp(pitch),
      onTapCancel: () => _noteUp(pitch),
      child: Container(
        margin: const EdgeInsets.symmetric(horizontal: 1.0),
        padding: const EdgeInsets.only(bottom: 6),
        alignment: Alignment.bottomCenter,
        decoration: BoxDecoration(
          color: held
              ? c.accentPrimary
              : (isBlack ? c.bgSunken : c.bgElevated),
          borderRadius: const BorderRadius.vertical(
            bottom: Radius.circular(FbRadius.sm),
          ),
          border: Border.all(
            color: held ? c.textPrimary : c.borderDefault,
          ),
        ),
        child: Text(
          label,
          style: FbTypography.monoReadout(
            c,
            size: 9.0,
            color: held
                ? c.textInverse
                : (isBlack ? c.textMuted : c.textPrimary),
          ),
        ),
      ),
    );
  }
}
