import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/scheduler.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../l10n/app_localizations.dart';
import '../music.dart';
import '../recorder.dart';
import '../theme.dart';
import '../ui/components.dart';

/// Focused recording screen: one big button to start, the same button to
/// stop. On stop the take is written to [targetPath] and the page pops with
/// that path; the editor then opens it. Closing discards.
class RecordPage extends StatefulWidget {
  final LiveRecorder recorder;
  final String targetPath;
  final String projectName;

  /// Key/scale of the project, so the live tuner reads in context.
  final String keyName;
  final String mode;

  const RecordPage({
    super.key,
    required this.recorder,
    required this.targetPath,
    required this.projectName,
    required this.keyName,
    required this.mode,
  });

  @override
  State<RecordPage> createState() => _RecordPageState();
}

enum _Phase { idle, recording, saving }

class _RecordPageState extends State<RecordPage>
    with SingleTickerProviderStateMixin, WidgetsBindingObserver {
  late final Ticker _ticker = createTicker((_) => _poll());
  _Phase _phase = _Phase.idle;
  RecorderStatus _status = const RecorderStatus();
  String? _error;
  bool _needsPermission = false;
  // Clip warning stays up briefly so it can actually be read.
  DateTime _clipUntil = DateTime.fromMillisecondsSinceEpoch(0);

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    _ticker.dispose();
    super.dispose();
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    // Android silences the mic in the background: keep what was sung.
    if (state == AppLifecycleState.paused && _phase == _Phase.recording) {
      _stopAndSave();
    }
  }

  Future<void> _start() async {
    setState(() {
      _error = null;
      _needsPermission = false;
    });
    if (!await widget.recorder.ensurePermission()) {
      if (mounted) setState(() => _needsPermission = true);
      return;
    }
    final error = widget.recorder.start();
    if (!mounted) return;
    if (error != null) {
      setState(() => _error = error);
      return;
    }
    setState(() => _phase = _Phase.recording);
    _ticker.start();
  }

  void _poll() {
    final status = widget.recorder.poll();
    if (status.clipped) {
      _clipUntil = DateTime.now().add(const Duration(milliseconds: 1200));
    }
    setState(() => _status = status);
    // Hit the length limit: the recorder stopped itself.
    if (!status.recording && _phase == _Phase.recording) _stopAndSave();
  }

  void _stopAndSave() {
    if (_phase != _Phase.recording) return;
    _ticker.stop();
    setState(() => _phase = _Phase.saving);
    final error = widget.recorder.save(widget.targetPath);
    if (error != null) {
      setState(() {
        _phase = _Phase.idle;
        _error = error;
      });
      return;
    }
    Navigator.of(context).pop(widget.targetPath);
  }

  Future<void> _close() async {
    if (_phase == _Phase.recording && _status.seconds > 2) {
      final l = AppLocalizations.of(context);
      final discard = await showDialog<bool>(
        context: context,
        builder: (dialogContext) => AlertDialog(
          backgroundColor: CantoColors.raised,
          title: Text(l.discardTakeTitle),
          content: Text(l.discardTakeBody),
          actions: [
            TextButton(
              onPressed: () => Navigator.pop(dialogContext, false),
              child: Text(l.keepRecording),
            ),
            TextButton(
              key: const Key('confirm-discard'),
              onPressed: () => Navigator.pop(dialogContext, true),
              style: TextButton.styleFrom(foregroundColor: CantoColors.heat),
              child: Text(l.discard),
            ),
          ],
        ),
      );
      if (discard != true || !mounted) return;
    }
    _ticker.stop();
    widget.recorder.discard();
    if (mounted) Navigator.of(context).pop();
  }

  @override
  Widget build(BuildContext context) {
    final l = AppLocalizations.of(context);
    final recording = _phase == _Phase.recording;
    final size = MediaQuery.sizeOf(context);
    final landscape = size.width > size.height && size.height < 600;
    return PopScope(
      canPop: false,
      onPopInvokedWithResult: (didPop, _) {
        if (!didPop) _close();
      },
      child: Scaffold(
        backgroundColor: CantoColors.surface,
        body: SafeArea(
          child: Column(
            children: [
              SizedBox(
                height: 56,
                child: Row(
                  children: [
                    const SizedBox(width: 4),
                    CantoIconButton(
                      key: const Key('record-close'),
                      icon: LucideIcons.x,
                      tooltip: MaterialLocalizations.of(context)
                          .closeButtonTooltip,
                      onTap: _phase == _Phase.saving ? null : _close,
                    ),
                    const SizedBox(width: 4),
                    Expanded(
                      child: Column(
                        mainAxisAlignment: MainAxisAlignment.center,
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Text(
                            widget.projectName,
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                            style: CantoTypography.sans(
                              size: 10.5,
                              weight: FontWeight.w600,
                              color: CantoColors.muted,
                            ),
                          ),
                          Text(
                            l.recordTake,
                            style: CantoTypography.sans(
                              size: 16,
                              weight: FontWeight.w600,
                              color: CantoColors.inkBright,
                            ),
                          ),
                        ],
                      ),
                    ),
                  ],
                ),
              ),
              // Landscape: tuner on the left, meter and button on the right
              // (the stacked layout doesn't fit a phone's landscape height).
              if (landscape)
                Expanded(
                  child: Row(
                    children: [
                      Expanded(child: _buildTuner(l, recording)),
                      SizedBox(
                        width: 320,
                        child: Column(
                          mainAxisAlignment: MainAxisAlignment.center,
                          children: [
                            _buildMeter(l),
                            const SizedBox(height: 20),
                            _buildButton(l),
                            const SizedBox(height: 10),
                            _buildHint(l, recording),
                          ],
                        ),
                      ),
                    ],
                  ),
                )
              else ...[
                Expanded(child: _buildTuner(l, recording)),
                _buildMeter(l),
                const SizedBox(height: 28),
                _buildButton(l),
                const SizedBox(height: 14),
                _buildHint(l, recording),
                const SizedBox(height: 16),
              ],
            ],
          ),
        ),
      ),
    );
  }

  Widget _buildHint(AppLocalizations l, bool recording) {
    return SizedBox(
      height: 40,
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 24),
        child: Text(
          _phase == _Phase.saving
              ? l.recordSaving
              : recording
              ? l.recordTapToStop
              : l.recordTapToStart,
          key: const Key('record-hint'),
          textAlign: TextAlign.center,
          style: CantoTypography.sans(size: 13, color: CantoColors.muted),
        ),
      ),
    );
  }

  /// Live tuner: what the singer is hitting, in the project's key.
  Widget _buildTuner(AppLocalizations l, bool recording) {
    final hz = recording ? _status.pitchHz : 0.0;
    final voiced = hz > 0;
    final midi = voiced ? 69 + 12 * math.log(hz / 440) / math.ln2 : 0.0;
    final reading = voiced
        ? centsFromTarget(midi, widget.keyName, widget.mode)
        : null;
    final off = reading != null && reading.cents.abs() >= 15;
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 24),
      child: Column(
        mainAxisAlignment: MainAxisAlignment.center,
        children: [
          if (_needsPermission || _error != null)
            _buildProblem(l)
          else ...[
            Text(
              reading != null ? spellMidi(reading.target, widget.keyName) : '—',
              key: const Key('record-note'),
              style: CantoTypography.num(
                size: 64,
                weight: FontWeight.w600,
                color: voiced ? CantoColors.inkBright : CantoColors.faint,
                height: 1.0,
              ),
            ),
            const SizedBox(height: 8),
            Text(
              reading != null
                  ? formatCents(reading.cents)
                  : (recording ? l.recordSingNote : ' '),
              style: CantoTypography.num(
                size: 16,
                weight: FontWeight.w600,
                color: reading == null
                    ? CantoColors.muted
                    : off
                    ? CantoColors.heat
                    : CantoColors.accent,
              ),
            ),
            const SizedBox(height: 18),
            SizedBox(width: 260, child: CentsGauge(cents: reading?.cents)),
            const SizedBox(height: 28),
            Text(
              formatClock(_status.seconds),
              key: const Key('record-time'),
              style: CantoTypography.num(
                size: 34,
                weight: FontWeight.w600,
                color: recording ? CantoColors.inkBright : CantoColors.muted,
              ),
            ),
          ],
        ],
      ),
    );
  }

  Widget _buildProblem(AppLocalizations l) {
    final message = _needsPermission
        ? l.recordNeedsMic
        : l.recordFailed(_error ?? '');
    return Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        const Icon(LucideIcons.micOff, size: 36, color: CantoColors.heat),
        const SizedBox(height: 12),
        Text(
          message,
          key: const Key('record-problem'),
          textAlign: TextAlign.center,
          style: CantoTypography.sans(size: 14, color: CantoColors.ink),
        ),
        if (_needsPermission) ...[
          const SizedBox(height: 14),
          CantoButton(
            key: const Key('record-allow-mic'),
            style: CantoButtonStyle.outline,
            icon: LucideIcons.mic,
            label: l.recordAllowMic,
            onTap: _start,
          ),
        ],
      ],
    );
  }

  Widget _buildMeter(AppLocalizations l) {
    final clipping = DateTime.now().isBefore(_clipUntil);
    final level = _status.level.clamp(0.0, 1.0);
    final color = clipping || level > 0.9
        ? CantoColors.meterHigh
        : level > 0.6
        ? CantoColors.meterMid
        : CantoColors.meterLow;
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 32),
      child: Column(
        children: [
          SizedBox(
            height: 18,
            child: clipping
                ? Text(
                    l.recordTooLoud,
                    key: const Key('record-too-loud'),
                    style: CantoTypography.sans(
                      size: 12,
                      weight: FontWeight.w600,
                      color: CantoColors.meterHigh,
                    ),
                  )
                : null,
          ),
          const SizedBox(height: 6),
          ClipRRect(
            borderRadius: BorderRadius.circular(4),
            child: SizedBox(
              height: 8,
              child: Stack(
                fit: StackFit.expand,
                children: [
                  const ColoredBox(color: CantoColors.high),
                  FractionallySizedBox(
                    alignment: Alignment.centerLeft,
                    // Square-root scale so quiet singing still moves the bar.
                    widthFactor: math.sqrt(level),
                    child: ColoredBox(color: color),
                  ),
                ],
              ),
            ),
          ),
        ],
      ),
    );
  }

  Widget _buildButton(AppLocalizations l) {
    final recording = _phase == _Phase.recording;
    return Pressable(
      key: const Key('record-btn'),
      semanticLabel: recording ? l.recordTapToStop : l.recordTapToStart,
      onTap: _phase == _Phase.saving
          ? null
          : recording
          ? _stopAndSave
          : _start,
      child: Container(
        width: 88,
        height: 88,
        decoration: BoxDecoration(
          shape: BoxShape.circle,
          border: Border.all(color: CantoColors.lineStrong, width: 3),
        ),
        alignment: Alignment.center,
        child: AnimatedContainer(
          duration: const Duration(milliseconds: 180),
          curve: Curves.easeOut,
          width: recording ? 32 : 68,
          height: recording ? 32 : 68,
          decoration: BoxDecoration(
            // Record uses the theme's arm red (state.arm), never the accent.
            color: CantoColors.heat,
            borderRadius: BorderRadius.circular(recording ? 6 : 34),
          ),
        ),
      ),
    );
  }
}
