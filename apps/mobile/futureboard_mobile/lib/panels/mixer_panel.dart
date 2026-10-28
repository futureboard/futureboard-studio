// Futureboard Studio Mixer Panel: Channel Strips, Inserts, Sends, Master LUFS & 128-Bin Spectrum.

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../engine/studio_controller.dart';
import '../models/studio_models.dart';
import '../theme/fb_tokens.dart';
import '../widgets/fb_controls.dart';

class MixerPanel extends StatelessWidget {
  final StudioController controller;

  const MixerPanel({super.key, required this.controller});

  @override
  Widget build(BuildContext context) {
    final c = controller.colors;
    final p = controller.project;
    final tel = controller.telemetry;

    return Container(
      color: c.bgPrimary,
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          // Scrollable Track & Bus Channel Strips
          Expanded(
            child: SingleChildScrollView(
              scrollDirection: Axis.horizontal,
              child: Row(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  for (final track in p.tracks)
                    _buildChannelStrip(context, track, c),
                ],
              ),
            ),
          ),

          // Pinned Master Bus Strip + Live FFT Spectrum & EBU R128 Loudness
          _buildMasterStrip(context, p.masterTrack, tel, c),
        ],
      ),
    );
  }

  Widget _buildChannelStrip(
      BuildContext context, TrackModel track, FbColors c) {
    final selected = controller.project.selectedTrackId == track.id;
    final trackColor = FbColors.parseHex(track.colorHex, c.accentPrimary);
    final meter = controller.telemetry.meterForTrack(track.id);

    return GestureDetector(
      onTap: () => controller.selectTrack(track.id),
      child: Container(
        width: 96,
        decoration: BoxDecoration(
          color: selected ? c.bgTertiary : c.bgSecondary,
          border: Border(
            right: BorderSide(color: c.borderDefault, width: 1.0),
          ),
        ),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            // Top Track Color Bar
            Container(height: 4, color: trackColor),

            // Track Title
            Container(
              height: 24,
              padding: const EdgeInsets.symmetric(horizontal: 6),
              alignment: Alignment.centerLeft,
              color: c.bgPrimary,
              child: Text(
                track.name,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: TextStyle(
                  fontFamily: FbTypography.fontSans,
                  fontSize: 11.0,
                  fontWeight: selected ? FontWeight.w700 : FontWeight.w600,
                  color: c.textPrimary,
                ),
              ),
            ),

            // Instrument / Insert Slots Rack
            Padding(
              padding: const EdgeInsets.all(4.0),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  if (track.instrument != null)
                    _buildPluginSlotPill(
                      trackId: track.id,
                      slotIndex: null,
                      slot: track.instrument!,
                      isInstrument: true,
                      accent: const Color(0xFF9B6BFF),
                      c: c,
                    ),
                  for (final ins in track.inserts.take(3))
                    Padding(
                      padding: const EdgeInsets.only(top: 2.0),
                      child: _buildPluginSlotPill(
                        trackId: track.id,
                        slotIndex: ins.slotIndex,
                        slot: ins,
                        isInstrument: false,
                        accent: c.accentPrimary,
                        c: c,
                      ),
                    ),
                  if (track.inserts.length < 4)
                    Padding(
                      padding: const EdgeInsets.only(top: 2.0),
                      child: _buildAddInsertMenu(context, track.id, c),
                    ),
                ],
              ),
            ),

            // Pan & Aux Send Knobs Row
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 2, vertical: 2),
              child: Row(
                children: [
                  Expanded(
                    child: FbRotaryKnob(
                      label: 'Pan',
                      value: track.pan,
                      min: -1.0,
                      max: 1.0,
                      defaultValue: 0.0,
                      colors: c,
                      accentColor: trackColor,
                      diameter: 28,
                      onChanged: (v) => controller.setTrackPan(track.id, v),
                    ),
                  ),
                  if (track.kind != TrackKind.bus)
                    Expanded(
                      child: FbRotaryKnob(
                        label: 'Aux',
                        value: track.sends.isNotEmpty
                            ? track.sends.first.levelDb
                            : -24.0,
                        min: -48.0,
                        max: 6.0,
                        defaultValue: -12.0,
                        unit: 'dB',
                        colors: c,
                        accentColor: const Color(0xFFD95B82),
                        diameter: 28,
                        onChanged: (v) =>
                            controller.setTrackSend(track.id, 'bus-1', v, true),
                      ),
                    ),
                ],
              ),
            ),

            // Latched M / S / R / I Row
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 4, vertical: 2),
              child: Row(
                mainAxisAlignment: MainAxisAlignment.spaceEvenly,
                children: [
                  FbLatchButton(
                    label: 'M',
                    tooltip: 'Mute',
                    active: track.muted,
                    latchColor: c.stateMute,
                    colors: c,
                    size: 19,
                    onTap: () => controller.toggleTrackMute(track.id),
                  ),
                  FbLatchButton(
                    label: 'S',
                    tooltip: 'Solo',
                    active: track.soloed,
                    latchColor: c.stateSolo,
                    colors: c,
                    size: 19,
                    onTap: () => controller.toggleTrackSolo(track.id),
                  ),
                  FbLatchButton(
                    label: 'R',
                    tooltip: 'Record Arm',
                    active: track.armed,
                    latchColor: c.stateArm,
                    colors: c,
                    size: 19,
                    onTap: () => controller.toggleTrackArm(track.id),
                  ),
                  FbLatchButton(
                    label: 'I',
                    tooltip: 'Input Monitor',
                    active: track.inputMonitoring,
                    latchColor: c.stateMonitor,
                    colors: c,
                    size: 19,
                    onTap: () => controller.toggleTrackMonitor(track.id),
                  ),
                ],
              ),
            ),

            // Vertical Fader + Stereo Meter
            Expanded(
              child: Padding(
                padding:
                    const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
                child: Row(
                  mainAxisAlignment: MainAxisAlignment.center,
                  children: [
                    Expanded(
                      child: RotatedBox(
                        quarterTurns: 3,
                        child: SliderTheme(
                          data: SliderTheme.of(context).copyWith(
                            trackHeight: 4.0,
                            thumbShape: const RoundSliderThumbShape(
                                enabledThumbRadius: 6.5),
                            overlayShape: SliderComponentShape.noOverlay,
                            activeTrackColor: trackColor,
                            inactiveTrackColor: c.bgSunken,
                            thumbColor: c.textPrimary,
                          ),
                          child: Slider(
                            value: track.volumeDb.clamp(-48.0, 12.0),
                            min: -48.0,
                            max: 12.0,
                            onChanged: (v) =>
                                controller.setTrackVolumeDb(track.id, v),
                          ),
                        ),
                      ),
                    ),
                    const SizedBox(width: 6),
                    FbStereoMeter(
                      peakLDb: meter.peakLDb,
                      peakRDb: meter.peakRDb,
                      rmsLDb: meter.rmsLDb,
                      rmsRDb: meter.rmsRDb,
                      gainReductionDb: meter.gainReductionDb,
                      colors: c,
                      width: 14,
                    ),
                  ],
                ),
              ),
            ),

            // Bottom dB Readout
            Container(
              height: 20,
              alignment: Alignment.center,
              color: c.bgSunken,
              child: Text(
                '${track.volumeDb.toStringAsFixed(1)} dB',
                style: FbTypography.monoReadout(
                  c,
                  size: 10.0,
                  color: meter.clipping ? c.meterRed : c.textSecondary,
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _buildMasterStrip(
    BuildContext context,
    TrackModel master,
    TelemetryModel tel,
    FbColors c,
  ) {
    return Container(
      width: 196,
      decoration: BoxDecoration(
        color: c.bgSecondary,
        border: Border(
          left: BorderSide(color: c.borderStrong, width: 1.5),
        ),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Container(height: 4, color: c.accentPrimary),
          Container(
            height: 24,
            padding: const EdgeInsets.symmetric(horizontal: 8),
            color: c.bgPrimary,
            child: Row(
              children: [
                Icon(LucideIcons.slidersVertical,
                    size: 12, color: c.accentPrimary),
                const SizedBox(width: 5),
                Text('MASTER BUS', style: FbTypography.sectionHeader(c)),
                const Spacer(),
                FbLatchButton(
                  label: 'M',
                  tooltip: 'Mute Master',
                  active: master.muted,
                  latchColor: c.stateMute,
                  colors: c,
                  size: 18,
                  onTap: () => controller.toggleTrackMute('master'),
                ),
              ],
            ),
          ),

          // Master Inserts
          Padding(
            padding: const EdgeInsets.all(4.0),
            child: Column(
              children: [
                for (final ins in master.inserts.take(3))
                  Padding(
                    padding: const EdgeInsets.only(bottom: 2.0),
                    child: _buildPluginSlotPill(
                      trackId: 'master',
                      slotIndex: ins.slotIndex,
                      slot: ins,
                      isInstrument: false,
                      accent: c.accentPrimary,
                      c: c,
                    ),
                  ),
              ],
            ),
          ),

          // Live 128-bin FFT Spectrum Mini-Canvas (square corners `FbRadius.none`)
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 6),
            child: SizedBox(
              height: 48,
              child: CustomPaint(
                painter: _MiniSpectrumPainter(
                  spectrumDb: tel.spectrumDb,
                  colors: c,
                ),
              ),
            ),
          ),

          // EBU R128 Loudness Telemetry Readout Grid + Master Fader
          Expanded(
            child: Padding(
              padding: const EdgeInsets.all(6.0),
              child: Row(
                children: [
                  // Loudness metrics column
                  Expanded(
                    child: Column(
                      mainAxisAlignment: MainAxisAlignment.spaceEvenly,
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        _lufsRow('LUFS-M', tel.lufsMomentary, 'LUFS', c),
                        _lufsRow('LUFS-S', tel.lufsShortTerm, 'LUFS', c),
                        _lufsRow('LUFS-I', tel.lufsIntegrated, 'LUFS', c),
                        _lufsRow('PEAK', tel.truePeakDbtp, 'dBTP', c),
                      ],
                    ),
                  ),
                  const SizedBox(width: 6),
                  // Master Fader
                  SizedBox(
                    width: 26,
                    child: RotatedBox(
                      quarterTurns: 3,
                      child: SliderTheme(
                        data: SliderTheme.of(context).copyWith(
                          trackHeight: 4.5,
                          thumbShape: const RoundSliderThumbShape(
                              enabledThumbRadius: 7.0),
                          overlayShape: SliderComponentShape.noOverlay,
                          activeTrackColor: c.accentPrimary,
                          inactiveTrackColor: c.bgSunken,
                          thumbColor: c.textPrimary,
                        ),
                        child: Slider(
                          value: master.volumeDb.clamp(-48.0, 12.0),
                          min: -48.0,
                          max: 12.0,
                          onChanged: (v) =>
                              controller.setTrackVolumeDb('master', v),
                        ),
                      ),
                    ),
                  ),
                  const SizedBox(width: 4),
                  FbStereoMeter(
                    peakLDb: tel.masterPeakLDb,
                    peakRDb: tel.masterPeakRDb,
                    rmsLDb: tel.masterRmsLDb,
                    rmsRDb: tel.masterRmsRDb,
                    colors: c,
                    width: 16,
                  ),
                ],
              ),
            ),
          ),

          Container(
            height: 20,
            alignment: Alignment.center,
            color: c.bgSunken,
            child: Text(
              'MASTER ${master.volumeDb.toStringAsFixed(1)} dB',
              style: FbTypography.monoReadout(
                c,
                size: 10.0,
                color: c.accentPrimary,
              ),
            ),
          ),
        ],
      ),
    );
  }

  Widget _lufsRow(String label, double val, String unit, FbColors c) {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 5, vertical: 2),
      decoration: BoxDecoration(
        color: c.bgSunken,
        borderRadius: BorderRadius.circular(FbRadius.xs),
        border: Border.all(color: c.borderSubtle),
      ),
      child: Row(
        children: [
          Text(label, style: FbTypography.micro(c)),
          const SizedBox(width: 4),
          Expanded(
            child: Text(
              '${val.toStringAsFixed(1)} $unit',
              textAlign: TextAlign.right,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: FbTypography.monoReadout(
                c,
                size: 9.0,
                color: val > -1.0 ? c.meterRed : c.textPrimary,
              ),
            ),
          ),
        ],
      ),
    );
  }

  Widget _buildPluginSlotPill({
    required String trackId,
    required int? slotIndex,
    required InsertSlotModel slot,
    required bool isInstrument,
    required Color accent,
    required FbColors c,
  }) {
    final isInspected = controller.inspectedPluginTrackId == trackId &&
        controller.inspectedPluginSlotIndex == slotIndex;

    return InkWell(
      onTap: () => controller.inspectPlugin(trackId, slotIndex),
      borderRadius: BorderRadius.circular(FbRadius.xs),
      child: Container(
        height: 20,
        padding: const EdgeInsets.symmetric(horizontal: 5),
        decoration: BoxDecoration(
          color: slot.enabled
              ? accent.withValues(alpha: isInspected ? 0.30 : 0.16)
              : c.bgSunken,
          borderRadius: BorderRadius.circular(FbRadius.xs),
          border: Border.all(
            color: isInspected
                ? accent
                : (slot.enabled
                    ? accent.withValues(alpha: 0.55)
                    : c.borderSubtle),
          ),
        ),
        child: Row(
          children: [
            GestureDetector(
              onTap: () => controller.togglePluginEnabled(
                  trackId, slotIndex, !slot.enabled),
              child: Container(
                width: 6,
                height: 6,
                decoration: BoxDecoration(
                  shape: BoxShape.circle,
                  color: slot.enabled ? accent : c.textMuted,
                ),
              ),
            ),
            const SizedBox(width: 4),
            Expanded(
              child: Text(
                slot.name,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: TextStyle(
                  fontFamily: FbTypography.fontSans,
                  fontSize: 9.5,
                  fontWeight: FontWeight.w600,
                  color: slot.enabled ? c.textPrimary : c.textMuted,
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _buildAddInsertMenu(
      BuildContext context, String trackId, FbColors c) {
    final effects =
        controller.catalog.where((p) => !p.isInstrument).toList();

    return PopupMenuButton<CatalogPluginModel>(
      tooltip: 'Add Effect Insert',
      color: c.bgElevated,
      onSelected: (plugin) {
        controller.selectTrack(trackId);
        controller.loadPluginFromBrowser(plugin);
      },
      itemBuilder: (context) => [
        for (final fx in effects)
          PopupMenuItem(
            value: fx,
            height: 28,
            child: Text(
              '${fx.name} (${fx.category})',
              style: FbTypography.body(c),
            ),
          ),
      ],
      child: Container(
        height: 18,
        alignment: Alignment.center,
        decoration: BoxDecoration(
          color: c.bgSunken,
          borderRadius: BorderRadius.circular(FbRadius.xs),
          border: Border.all(color: c.borderSubtle),
        ),
        child: Text(
          '+ Insert FX',
          style: FbTypography.micro(c),
        ),
      ),
    );
  }
}

class _MiniSpectrumPainter extends CustomPainter {
  final List<double> spectrumDb;
  final FbColors colors;

  _MiniSpectrumPainter({required this.spectrumDb, required this.colors});

  @override
  void paint(Canvas canvas, Size size) {
    canvas.drawRect(Offset.zero & size, Paint()..color = colors.bgSunken);
    if (spectrumDb.isEmpty) return;

    final path = Path()..moveTo(0, size.height);
    for (int i = 0; i < spectrumDb.length; i++) {
      final x = (i / (spectrumDb.length - 1)) * size.width;
      final norm = ((spectrumDb[i] + 90.0) / 90.0).clamp(0.0, 1.0);
      final y = size.height * (1.0 - norm);
      path.lineTo(x, y);
    }
    path.lineTo(size.width, size.height);
    path.close();

    canvas.drawPath(
      path,
      Paint()..color = colors.accentPrimary.withValues(alpha: 0.28),
    );

    final strokePath = Path();
    for (int i = 0; i < spectrumDb.length; i++) {
      final x = (i / (spectrumDb.length - 1)) * size.width;
      final norm = ((spectrumDb[i] + 90.0) / 90.0).clamp(0.0, 1.0);
      final y = size.height * (1.0 - norm);
      if (i == 0) {
        strokePath.moveTo(x, y);
      } else {
        strokePath.lineTo(x, y);
      }
    }
    canvas.drawPath(
      strokePath,
      Paint()
        ..color = colors.accentPrimary
        ..style = PaintingStyle.stroke
        ..strokeWidth = 1.3,
    );
  }

  @override
  bool shouldRepaint(covariant _MiniSpectrumPainter oldDelegate) => true;
}
