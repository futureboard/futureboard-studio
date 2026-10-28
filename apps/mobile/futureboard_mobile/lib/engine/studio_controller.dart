// State controller & real-time audio pump connecting Flutter UI to `RustStudioFfi`.

import 'dart:async';
import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

import '../models/studio_models.dart';
import '../theme/fb_tokens.dart';
import 'rust_studio_ffi.dart';

enum StudioDockTab {
  mixer,
  pianoRoll,
  pluginEditor,
  perform,
  inspector,
}

enum StudioLayoutPreset {
  arrange,
  mix,
  edit,
  perform,
}

class StudioController extends ChangeNotifier {
  static const MethodChannel _audioChannel = MethodChannel('futureboard/audio');

  late final RustStudioFfi _ffi;
  late ProjectModel project;
  late TelemetryModel telemetry;
  late List<CatalogPluginModel> catalog;

  bool isDarkTheme = true;
  bool isBrowserOpen = false;
  bool isInspectorOpen = true;
  bool isBottomDockOpen = true;
  StudioDockTab activeDockTab = StudioDockTab.mixer;
  StudioLayoutPreset activePreset = StudioLayoutPreset.arrange;

  // Currently inspected plugin target: (trackId, slotIndex: null = instrument, >=0 = insert)
  String inspectedPluginTrackId = 'track-4';
  int? inspectedPluginSlotIndex = 0;

  double timelineZoomPxPerBeat = 34.0;
  double pianoRollZoomPxPerBeat = 48.0;
  double quantizeGridBeats = 0.25;
  int performOctave = 3; // C3 base
  String statusBanner = 'Engine Ready • 27 Built-In Rust DSP Plugins Loaded';

  Timer? _audioPumpTimer;
  bool _audioTrackStarted = false;
  bool _disposed = false;

  FbColors get colors => isDarkTheme ? FbColors.dark : FbColors.light;

  StudioController({bool startAudioPump = true}) {
    _ffi = RustStudioFfi(sampleRate: 48000);
    catalog = _ffi
        .getCatalogList()
        .map((e) => CatalogPluginModel.fromJson(e as Map<String, dynamic>))
        .toList();
    project = ProjectModel.fromJson(_ffi.getSnapshotMap());
    telemetry = TelemetryModel.fromJson(_ffi.getTelemetryMap());

    if (startAudioPump) {
      _initAudioStream();
    }
  }

  Future<void> _initAudioStream() async {
    try {
      await _audioChannel.invokeMethod<int>('start', {'sampleRate': 48000});
      _audioTrackStarted = true;
    } catch (_) {
      // Running in desktop / widget test environment without Android AudioTrack
      _audioTrackStarted = false;
    }
    _audioPumpTimer = Timer.periodic(const Duration(milliseconds: 24), (_) {
      _pumpAudioTick();
    });
  }

  void _pumpAudioTick() {
    if (_disposed || !_ffi.isLoaded) return;
    // Render 1152 frames (~24 ms at 48 kHz) per tick
    final pcmBytes = _ffi.renderPcm16Stereo(1152);
    if (_audioTrackStarted && pcmBytes.isNotEmpty) {
      _audioChannel.invokeMethod('writePcm', {'pcm': pcmBytes}).catchError((_) {});
    }
    telemetry = TelemetryModel.fromJson(_ffi.getTelemetryMap());
    notifyListeners();
  }

  /// Manually renders one block (useful in unit/widget tests when timer pump is disabled).
  void stepEngineForTest({int frames = 1024}) {
    _ffi.renderPcm16Stereo(frames);
    refreshSnapshot();
  }

  void refreshSnapshot() {
    if (_disposed || !_ffi.isLoaded) return;
    project = ProjectModel.fromJson(_ffi.getSnapshotMap());
    telemetry = TelemetryModel.fromJson(_ffi.getTelemetryMap());
    notifyListeners();
  }

  TrackModel get selectedTrack {
    for (final t in project.tracks) {
      if (t.id == project.selectedTrackId) return t;
    }
    if (project.selectedTrackId == 'master') return project.masterTrack;
    return project.tracks.isNotEmpty ? project.tracks.first : project.masterTrack;
  }

  TrackModel get inspectedPluginTrack {
    if (inspectedPluginTrackId == 'master') return project.masterTrack;
    for (final t in project.tracks) {
      if (t.id == inspectedPluginTrackId) return t;
    }
    return selectedTrack;
  }

  InsertSlotModel? get inspectedPluginSlot {
    final t = inspectedPluginTrack;
    if (inspectedPluginSlotIndex == null) {
      return t.instrument;
    }
    final idx = inspectedPluginSlotIndex!;
    if (idx >= 0 && idx < t.inserts.length) {
      return t.inserts[idx];
    }
    return t.instrument ?? (t.inserts.isNotEmpty ? t.inserts.first : null);
  }

  MidiClipModel? get selectedMidiClip {
    final cid = project.selectedClipId;
    for (final t in project.tracks) {
      for (final mc in t.midiClips) {
        if (mc.id == cid) return mc;
      }
    }
    final st = selectedTrack;
    return st.midiClips.isNotEmpty ? st.midiClips.first : null;
  }

  AudioClipModel? get selectedAudioClip {
    final cid = project.selectedClipId;
    for (final t in project.tracks) {
      for (final ac in t.audioClips) {
        if (ac.id == cid) return ac;
      }
    }
    final st = selectedTrack;
    return st.audioClips.isNotEmpty ? st.audioClips.first : null;
  }

  // ── UI Theme & Workspace Layout Controls ──────────────────────────────────

  void toggleTheme() {
    isDarkTheme = !isDarkTheme;
    notifyListeners();
  }

  void toggleBrowser() {
    isBrowserOpen = !isBrowserOpen;
    notifyListeners();
  }

  void toggleInspector() {
    isInspectorOpen = !isInspectorOpen;
    notifyListeners();
  }

  void toggleBottomDock() {
    isBottomDockOpen = !isBottomDockOpen;
    notifyListeners();
  }

  void setDockTab(StudioDockTab tab) {
    activeDockTab = tab;
    isBottomDockOpen = true;
    notifyListeners();
  }

  void applyLayoutPreset(StudioLayoutPreset preset) {
    activePreset = preset;
    switch (preset) {
      case StudioLayoutPreset.arrange:
        isBottomDockOpen = true;
        activeDockTab = StudioDockTab.mixer;
        break;
      case StudioLayoutPreset.mix:
        isBottomDockOpen = true;
        activeDockTab = StudioDockTab.mixer;
        break;
      case StudioLayoutPreset.edit:
        isBottomDockOpen = true;
        activeDockTab = selectedTrack.kind == TrackKind.audio
            ? StudioDockTab.inspector
            : StudioDockTab.pianoRoll;
        break;
      case StudioLayoutPreset.perform:
        isBottomDockOpen = true;
        activeDockTab = StudioDockTab.perform;
        break;
    }
    notifyListeners();
  }

  void setTimelineZoom(double pxPerBeat) {
    timelineZoomPxPerBeat = pxPerBeat.clamp(16.0, 96.0);
    notifyListeners();
  }

  void setPianoRollZoom(double pxPerBeat) {
    pianoRollZoomPxPerBeat = pxPerBeat.clamp(24.0, 120.0);
    notifyListeners();
  }

  // ── Transport & Project Actions ───────────────────────────────────────────

  void togglePlay() {
    final next = !project.isPlaying;
    _ffi.setPlaying(next);
    if (!next && _audioTrackStarted) {
      _audioChannel.invokeMethod('flush').catchError((_) {});
    }
    statusBanner = next ? 'Transport Playing' : 'Transport Stopped';
    refreshSnapshot();
  }

  void stopTransport() {
    _ffi.setPlaying(false);
    _ffi.seekBeats(0.0);
    if (_audioTrackStarted) {
      _audioChannel.invokeMethod('flush').catchError((_) {});
    }
    statusBanner = 'Stopped • Returned to 1.1.1';
    refreshSnapshot();
  }

  void toggleRecord() {
    final next = !project.isRecording;
    _ffi.setRecording(next);
    statusBanner = next
        ? 'Recording Live Input to Armed Track...'
        : 'Recording Committed to Audio Clip';
    refreshSnapshot();
  }

  void seekBeats(double beats) {
    _ffi.seekBeats(beats.clamp(0.0, 256.0));
    refreshSnapshot();
  }

  void setBpm(double bpm) {
    _ffi.setBpm(bpm.clamp(40.0, 240.0));
    refreshSnapshot();
  }

  void toggleLoop() {
    _ffi.dispatchAction({
      'action': 'setLoop',
      'enabled': !project.loopEnabled,
      'startBeats': project.loopStartBeats,
      'endBeats': project.loopEndBeats,
    });
    refreshSnapshot();
  }

  void setLoopRange(double startBeats, double endBeats) {
    _ffi.dispatchAction({
      'action': 'setLoop',
      'enabled': project.loopEnabled,
      'startBeats': startBeats,
      'endBeats': endBeats,
    });
    refreshSnapshot();
  }

  void toggleMetronome() {
    _ffi.dispatchAction({
      'action': 'setMetronome',
      'enabled': !project.metronomeEnabled,
    });
    refreshSnapshot();
  }

  void toggleMasterBinaural() {
    final next = !project.masterBinauralEnabled;
    _ffi.dispatchAction({
      'action': 'setMasterBinaural',
      'enabled': next,
      'reverbMix': project.roomReverbMix,
    });
    statusBanner = next
        ? 'MIT KEMAR Binaural 3D Headphone Monitoring Active'
        : 'Standard Stereo Bus Monitoring';
    refreshSnapshot();
  }

  void undo() {
    final res = _ffi.dispatchAction({'action': 'undo'});
    if (res['ok'] == true) {
      statusBanner = 'Undo Applied';
    }
    refreshSnapshot();
  }

  void redo() {
    final res = _ffi.dispatchAction({'action': 'redo'});
    if (res['ok'] == true) {
      statusBanner = 'Redo Applied';
    }
    refreshSnapshot();
  }

  void loadDemoProject() {
    _ffi.dispatchAction({'action': 'loadDemo'});
    statusBanner = 'Loaded Neon Horizon Demo Session';
    refreshSnapshot();
  }

  Future<String?> exportMasterWav() async {
    String outPath = 'Futureboard_Master_Bounce.wav';
    try {
      final p = await _audioChannel
          .invokeMethod<String>('getExportPath', {'fileName': outPath});
      if (p != null && p.isNotEmpty) outPath = p;
    } catch (_) {}
    final res = _ffi.dispatchAction({
      'action': 'exportWav',
      'outputPath': outPath,
      'totalBeats': project.loopEndBeats.clamp(4.0, 64.0),
    });
    if (res['ok'] == true) {
      statusBanner = 'Bounced Master WAV -> $outPath';
      notifyListeners();
      return outPath;
    }
    statusBanner = 'WAV Export Failed: ${res['error'] ?? 'unknown'}';
    notifyListeners();
    return null;
  }

  // ── Track & Mixer Actions ─────────────────────────────────────────────────

  void selectTrack(String trackId, {String? clipId}) {
    _ffi.dispatchAction({
      'action': 'selectTrack',
      'trackId': trackId,
      'clipId': clipId,
    });
    inspectedPluginTrackId = trackId;
    refreshSnapshot();
  }

  void addTrack(String name, TrackKind kind, {String? instrumentId}) {
    final res = _ffi.dispatchAction({
      'action': 'addTrack',
      'name': name,
      'kind': kind.toJson(),
      'instrumentId': instrumentId,
    });
    if (res['ok'] == true) {
      statusBanner = 'Created Track "$name"';
    }
    refreshSnapshot();
  }

  void deleteTrack(String trackId) {
    _ffi.dispatchAction({
      'action': 'deleteTrack',
      'trackId': trackId,
    });
    statusBanner = 'Deleted Track';
    refreshSnapshot();
  }

  void setTrackVolumeDb(String trackId, double volumeDb) {
    _ffi.setTrackMixer(trackId: trackId, volumeDb: volumeDb);
    refreshSnapshot();
  }

  void setTrackPan(String trackId, double pan) {
    _ffi.setTrackMixer(trackId: trackId, pan: pan);
    refreshSnapshot();
  }

  void toggleTrackMute(String trackId) {
    final t = trackId == 'master'
        ? project.masterTrack
        : project.tracks.firstWhere((e) => e.id == trackId);
    _ffi.setTrackMixer(trackId: trackId, muted: !t.muted);
    refreshSnapshot();
  }

  void toggleTrackSolo(String trackId) {
    final t = trackId == 'master'
        ? project.masterTrack
        : project.tracks.firstWhere((e) => e.id == trackId);
    _ffi.setTrackMixer(trackId: trackId, soloed: !t.soloed);
    refreshSnapshot();
  }

  void toggleTrackArm(String trackId) {
    final t = trackId == 'master'
        ? project.masterTrack
        : project.tracks.firstWhere((e) => e.id == trackId);
    _ffi.setTrackMixer(trackId: trackId, armed: !t.armed);
    refreshSnapshot();
  }

  void toggleTrackMonitor(String trackId) {
    final t = trackId == 'master'
        ? project.masterTrack
        : project.tracks.firstWhere((e) => e.id == trackId);
    _ffi.setTrackMixer(trackId: trackId, inputMonitoring: !t.inputMonitoring);
    refreshSnapshot();
  }

  void setTrackSend(String trackId, String busId, double levelDb, bool enabled) {
    _ffi.dispatchAction({
      'action': 'setTrackSend',
      'trackId': trackId,
      'busId': busId,
      'levelDb': levelDb,
      'enabled': enabled,
    });
    refreshSnapshot();
  }

  void setTrackSpatial(
    String trackId, {
    required double x,
    required double y,
    required double z,
    required bool binaural,
  }) {
    _ffi.dispatchAction({
      'action': 'setTrackSpatial',
      'trackId': trackId,
      'x': x,
      'y': y,
      'z': z,
      'binaural': binaural,
    });
    refreshSnapshot();
  }

  // ── Plugin Chain & Editor Actions ─────────────────────────────────────────

  void inspectPlugin(String trackId, int? slotIndex, {bool openEditor = true}) {
    inspectedPluginTrackId = trackId;
    inspectedPluginSlotIndex = slotIndex;
    _ffi.dispatchAction({
      'action': 'inspectPlugin',
      'trackId': trackId,
      'slotIndex': slotIndex,
    });
    if (openEditor) {
      activeDockTab = StudioDockTab.pluginEditor;
      isBottomDockOpen = true;
    }
    refreshSnapshot();
  }

  void loadPluginFromBrowser(CatalogPluginModel plugin) {
    final targetTrack = selectedTrack;
    if (plugin.isInstrument) {
      if (targetTrack.kind == TrackKind.instrument ||
          targetTrack.kind == TrackKind.drumKit) {
        _ffi.dispatchAction({
          'action': 'setTrackInstrument',
          'trackId': targetTrack.id,
          'pluginId': plugin.id,
        });
        inspectPlugin(targetTrack.id, null, openEditor: true);
        statusBanner = 'Loaded ${plugin.name} on ${targetTrack.name}';
      } else {
        final res = _ffi.dispatchAction({
          'action': 'addTrack',
          'name': plugin.name,
          'kind': plugin.id == 'drumsampler' ? 'drumKit' : 'instrument',
          'instrumentId': plugin.id,
        });
        final newTid = res['trackId'] as String? ?? selectedTrack.id;
        inspectPlugin(newTid, null, openEditor: true);
        statusBanner = 'Created ${plugin.name} Track';
      }
    } else {
      final res = _ffi.dispatchAction({
        'action': 'addTrackInsert',
        'trackId': targetTrack.id,
        'pluginId': plugin.id,
      });
      final slotIdx = (res['slotIndex'] as num?)?.toInt() ?? 0;
      inspectPlugin(targetTrack.id, slotIdx, openEditor: true);
      statusBanner = 'Inserted ${plugin.name} on ${targetTrack.name}';
    }
    refreshSnapshot();
  }

  void removeTrackInsert(String trackId, int slotIndex) {
    _ffi.dispatchAction({
      'action': 'removeTrackInsert',
      'trackId': trackId,
      'slotIndex': slotIndex,
    });
    refreshSnapshot();
  }

  void togglePluginEnabled(String trackId, int? slotIndex, bool enabled) {
    _ffi.dispatchAction({
      'action': 'setPluginEnabled',
      'trackId': trackId,
      'slotIndex': slotIndex,
      'enabled': enabled,
    });
    refreshSnapshot();
  }

  void setPluginParam(
    String trackId,
    int? slotIndex,
    String paramId,
    double value,
  ) {
    _ffi.setPluginParam(
      trackId: trackId,
      slotIndex: slotIndex,
      paramId: paramId,
      value: value,
    );
    refreshSnapshot();
  }

  void applyPluginPreset(String trackId, int? slotIndex, String presetName) {
    _ffi.dispatchAction({
      'action': 'applyPluginPreset',
      'trackId': trackId,
      'slotIndex': slotIndex,
      'presetName': presetName,
    });
    statusBanner = 'Preset "$presetName" Applied';
    refreshSnapshot();
  }

  // ── Arrangement Clips, Piano Roll & Live MIDI ─────────────────────────────

  void addMidiClip(String trackId, double startBeat, {double lengthBeats = 4.0}) {
    _ffi.dispatchAction({
      'action': 'addMidiClip',
      'trackId': trackId,
      'startBeat': startBeat,
      'lengthBeats': lengthBeats,
    });
    statusBanner = 'Created MIDI Clip';
    refreshSnapshot();
  }

  Future<void> pickAndImportAudioFile(String trackId, double startBeat) async {
    try {
      final path = await _audioChannel.invokeMethod<String>('pickAudioFile');
      if (path != null && path.isNotEmpty) {
        final res = _ffi.dispatchAction({
          'action': 'importAudioFile',
          'trackId': trackId,
          'filePath': path,
          'startBeat': startBeat,
        });
        if (res['ok'] == true) {
          statusBanner = 'Imported Audio Clip';
        } else {
          statusBanner = 'Import Error: ${res['error']}';
        }
        refreshSnapshot();
      }
    } catch (e) {
      statusBanner = 'Audio File Picker Unavailable: $e';
      notifyListeners();
    }
  }

  void updateClipBounds(
    String trackId,
    String clipId, {
    required double startBeat,
    required double lengthBeats,
    double? gainDb,
    double? pitchSemitones,
  }) {
    _ffi.dispatchAction({
      'action': 'updateClipBounds',
      'trackId': trackId,
      'clipId': clipId,
      'startBeat': startBeat,
      'lengthBeat': lengthBeats,
      'lengthBeats': lengthBeats,
      'gainDb': gainDb,
      'pitchSemitones': pitchSemitones,
    });
    refreshSnapshot();
  }

  void splitSelectedClipAtPlayhead() {
    final cid = project.selectedClipId;
    if (cid == null) return;
    final res = _ffi.dispatchAction({
      'action': 'splitClip',
      'trackId': project.selectedTrackId,
      'clipId': cid,
      'splitBeat': telemetry.playheadBeats,
    });
    if (res['ok'] == true) {
      statusBanner = 'Split Clip at Beat ${telemetry.playheadBeats.toStringAsFixed(2)}';
    }
    refreshSnapshot();
  }

  void deleteSelectedClip() {
    final cid = project.selectedClipId;
    if (cid == null) return;
    _ffi.dispatchAction({
      'action': 'deleteClip',
      'trackId': project.selectedTrackId,
      'clipId': cid,
    });
    statusBanner = 'Deleted Clip';
    refreshSnapshot();
  }

  void upsertMidiNote({
    required String trackId,
    required String clipId,
    String? noteId,
    required int pitch,
    required double startBeat,
    required double durationBeats,
    required int velocity,
  }) {
    _ffi.dispatchAction({
      'action': 'upsertMidiNote',
      'trackId': trackId,
      'clipId': clipId,
      'noteId': noteId,
      'pitch': pitch,
      'startBeat': startBeat,
      'durationBeats': durationBeats,
      'velocity': velocity,
    });
    refreshSnapshot();
  }

  void deleteMidiNote({
    required String trackId,
    required String clipId,
    required String noteId,
  }) {
    _ffi.dispatchAction({
      'action': 'deleteMidiNote',
      'trackId': trackId,
      'clipId': clipId,
      'noteId': noteId,
    });
    refreshSnapshot();
  }

  void quantizeClip(String trackId, String clipId, [double? gridBeats]) {
    _ffi.dispatchAction({
      'action': 'quantizeMidiClip',
      'trackId': trackId,
      'clipId': clipId,
      'gridBeats': gridBeats ?? quantizeGridBeats,
    });
    statusBanner = 'Quantized MIDI Notes to 1/${(4.0 / (gridBeats ?? quantizeGridBeats)).round()}';
    refreshSnapshot();
  }

  void triggerLiveMidi(int note, int velocity, bool isNoteOn, {String? trackId}) {
    _ffi.triggerMidi(
      trackId ?? project.selectedTrackId,
      note.clamp(0, 127),
      velocity.clamp(0, 127),
      isNoteOn,
    );
  }

  @override
  void dispose() {
    _disposed = true;
    _audioPumpTimer?.cancel();
    if (_audioTrackStarted) {
      _audioChannel.invokeMethod('stop').catchError((_) {});
    }
    _ffi.dispose();
    super.dispose();
  }
}
