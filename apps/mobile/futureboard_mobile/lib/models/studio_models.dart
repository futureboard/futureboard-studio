// Strongly-typed Dart models mirroring `fb_studio_mobile_engine` Rust structs.

enum TrackKind {
  audio,
  instrument,
  drumKit,
  bus,
  master;

  static TrackKind fromJson(String raw) {
    switch (raw.toLowerCase()) {
      case 'audio':
        return TrackKind.audio;
      case 'drumkit':
        return TrackKind.drumKit;
      case 'bus':
        return TrackKind.bus;
      case 'master':
        return TrackKind.master;
      default:
        return TrackKind.instrument;
    }
  }

  String toJson() {
    switch (this) {
      case TrackKind.audio:
        return 'audio';
      case TrackKind.instrument:
        return 'instrument';
      case TrackKind.drumKit:
        return 'drumKit';
      case TrackKind.bus:
        return 'bus';
      case TrackKind.master:
        return 'master';
    }
  }
}

class MidiNoteModel {
  final String id;
  final int pitch;
  final double startBeat;
  final double durationBeats;
  final int velocity;
  final bool muted;

  const MidiNoteModel({
    required this.id,
    required this.pitch,
    required this.startBeat,
    required this.durationBeats,
    required this.velocity,
    this.muted = false,
  });

  factory MidiNoteModel.fromJson(Map<String, dynamic> j) => MidiNoteModel(
        id: j['id'] as String? ?? 'note',
        pitch: (j['pitch'] as num? ?? 60).toInt(),
        startBeat: (j['startBeat'] as num? ?? 0.0).toDouble(),
        durationBeats: (j['durationBeats'] as num? ?? 1.0).toDouble(),
        velocity: (j['velocity'] as num? ?? 100).toInt(),
        muted: j['muted'] as bool? ?? false,
      );
}

class MidiClipModel {
  final String id;
  final String name;
  final double startBeat;
  final double lengthBeats;
  final double loopLengthBeats;
  final bool loopEnabled;
  final String colorHex;
  final List<MidiNoteModel> notes;
  final bool muted;

  const MidiClipModel({
    required this.id,
    required this.name,
    required this.startBeat,
    required this.lengthBeats,
    required this.loopLengthBeats,
    required this.loopEnabled,
    required this.colorHex,
    required this.notes,
    this.muted = false,
  });

  factory MidiClipModel.fromJson(Map<String, dynamic> j) => MidiClipModel(
        id: j['id'] as String? ?? 'clip-m',
        name: j['name'] as String? ?? 'MIDI Clip',
        startBeat: (j['startBeat'] as num? ?? 0.0).toDouble(),
        lengthBeats: (j['lengthBeats'] as num? ?? 4.0).toDouble(),
        loopLengthBeats: (j['loopLengthBeats'] as num? ?? 4.0).toDouble(),
        loopEnabled: j['loopEnabled'] as bool? ?? true,
        colorHex: j['colorHex'] as String? ?? '#4C8DFF',
        notes: (j['notes'] as List<dynamic>? ?? const [])
            .map((e) => MidiNoteModel.fromJson(e as Map<String, dynamic>))
            .toList(),
        muted: j['muted'] as bool? ?? false,
      );
}

class AudioClipModel {
  final String id;
  final String name;
  final String sourceId;
  final String sourcePath;
  final double startBeat;
  final double lengthBeats;
  final double offsetBeats;
  final double gainDb;
  final double pitchSemitones;
  final double fadeInBeats;
  final double fadeOutBeats;
  final String colorHex;
  final List<double> waveformPeaks;
  final bool muted;

  const AudioClipModel({
    required this.id,
    required this.name,
    required this.sourceId,
    required this.sourcePath,
    required this.startBeat,
    required this.lengthBeats,
    required this.offsetBeats,
    required this.gainDb,
    required this.pitchSemitones,
    required this.fadeInBeats,
    required this.fadeOutBeats,
    required this.colorHex,
    required this.waveformPeaks,
    this.muted = false,
  });

  factory AudioClipModel.fromJson(Map<String, dynamic> j) => AudioClipModel(
        id: j['id'] as String? ?? 'clip-a',
        name: j['name'] as String? ?? 'Audio Clip',
        sourceId: j['sourceId'] as String? ?? '',
        sourcePath: j['sourcePath'] as String? ?? '',
        startBeat: (j['startBeat'] as num? ?? 0.0).toDouble(),
        lengthBeats: (j['lengthBeats'] as num? ?? 4.0).toDouble(),
        offsetBeats: (j['offsetBeats'] as num? ?? 0.0).toDouble(),
        gainDb: (j['gainDb'] as num? ?? 0.0).toDouble(),
        pitchSemitones: (j['pitchSemitones'] as num? ?? 0.0).toDouble(),
        fadeInBeats: (j['fadeInBeats'] as num? ?? 0.0).toDouble(),
        fadeOutBeats: (j['fadeOutBeats'] as num? ?? 0.0).toDouble(),
        colorHex: j['colorHex'] as String? ?? '#38B87C',
        waveformPeaks: (j['waveformPeaks'] as List<dynamic>? ?? const [])
            .map((e) => (e as num).toDouble())
            .toList(),
        muted: j['muted'] as bool? ?? false,
      );
}

class PluginParamModel {
  final String id;
  final String name;
  final double value;
  final double defaultValue;
  final double min;
  final double max;
  final String unit;
  final String group;

  const PluginParamModel({
    required this.id,
    required this.name,
    required this.value,
    required this.defaultValue,
    required this.min,
    required this.max,
    required this.unit,
    required this.group,
  });

  factory PluginParamModel.fromJson(Map<String, dynamic> j) => PluginParamModel(
        id: j['id'] as String? ?? '',
        name: j['name'] as String? ?? '',
        value: (j['value'] as num? ?? 0.0).toDouble(),
        defaultValue: (j['defaultValue'] as num? ?? 0.0).toDouble(),
        min: (j['min'] as num? ?? 0.0).toDouble(),
        max: (j['max'] as num? ?? 1.0).toDouble(),
        unit: j['unit'] as String? ?? '',
        group: j['group'] as String? ?? 'General',
      );
}

class InsertSlotModel {
  final int slotIndex;
  final String pluginId;
  final String name;
  final bool enabled;
  final String activePreset;
  final List<PluginParamModel> params;

  const InsertSlotModel({
    required this.slotIndex,
    required this.pluginId,
    required this.name,
    required this.enabled,
    required this.activePreset,
    required this.params,
  });

  factory InsertSlotModel.fromJson(Map<String, dynamic> j) => InsertSlotModel(
        slotIndex: (j['slotIndex'] as num? ?? 0).toInt(),
        pluginId: j['pluginId'] as String? ?? '',
        name: j['name'] as String? ?? '',
        enabled: j['enabled'] as bool? ?? true,
        activePreset: j['activePreset'] as String? ?? 'Default',
        params: (j['params'] as List<dynamic>? ?? const [])
            .map((e) => PluginParamModel.fromJson(e as Map<String, dynamic>))
            .toList(),
      );

  double paramValue(String id, [double fallback = 0.0]) {
    for (final p in params) {
      if (p.id == id) return p.value;
    }
    return fallback;
  }
}

class SendSlotModel {
  final String targetBusId;
  final double levelDb;
  final bool enabled;
  final bool preFader;

  const SendSlotModel({
    required this.targetBusId,
    required this.levelDb,
    required this.enabled,
    required this.preFader,
  });

  factory SendSlotModel.fromJson(Map<String, dynamic> j) => SendSlotModel(
        targetBusId: j['targetBusId'] as String? ?? 'bus-1',
        levelDb: (j['levelDb'] as num? ?? -12.0).toDouble(),
        enabled: j['enabled'] as bool? ?? true,
        preFader: j['preFader'] as bool? ?? false,
      );
}

class TrackModel {
  final String id;
  final String name;
  final TrackKind kind;
  final String colorHex;
  final double volumeDb;
  final double pan;
  final bool muted;
  final bool soloed;
  final bool armed;
  final bool inputMonitoring;
  final bool automationRead;
  final double spatialX;
  final double spatialY;
  final double spatialZ;
  final bool spatialBinaural;
  final InsertSlotModel? instrument;
  final List<InsertSlotModel> inserts;
  final List<SendSlotModel> sends;
  final List<MidiClipModel> midiClips;
  final List<AudioClipModel> audioClips;

  const TrackModel({
    required this.id,
    required this.name,
    required this.kind,
    required this.colorHex,
    required this.volumeDb,
    required this.pan,
    required this.muted,
    required this.soloed,
    required this.armed,
    required this.inputMonitoring,
    required this.automationRead,
    required this.spatialX,
    required this.spatialY,
    required this.spatialZ,
    required this.spatialBinaural,
    required this.instrument,
    required this.inserts,
    required this.sends,
    required this.midiClips,
    required this.audioClips,
  });

  factory TrackModel.fromJson(Map<String, dynamic> j) => TrackModel(
        id: j['id'] as String? ?? 'track-1',
        name: j['name'] as String? ?? 'Track',
        kind: TrackKind.fromJson(j['kind'] as String? ?? 'instrument'),
        colorHex: j['colorHex'] as String? ?? '#4C8DFF',
        volumeDb: (j['volumeDb'] as num? ?? 0.0).toDouble(),
        pan: (j['pan'] as num? ?? 0.0).toDouble(),
        muted: j['muted'] as bool? ?? false,
        soloed: j['soloed'] as bool? ?? false,
        armed: j['armed'] as bool? ?? false,
        inputMonitoring: j['inputMonitoring'] as bool? ?? false,
        automationRead: j['automationRead'] as bool? ?? true,
        spatialX: (j['spatialX'] as num? ?? 0.0).toDouble(),
        spatialY: (j['spatialY'] as num? ?? 0.85).toDouble(),
        spatialZ: (j['spatialZ'] as num? ?? 0.0).toDouble(),
        spatialBinaural: j['spatialBinaural'] as bool? ?? false,
        instrument: j['instrument'] != null
            ? InsertSlotModel.fromJson(j['instrument'] as Map<String, dynamic>)
            : null,
        inserts: (j['inserts'] as List<dynamic>? ?? const [])
            .map((e) => InsertSlotModel.fromJson(e as Map<String, dynamic>))
            .toList(),
        sends: (j['sends'] as List<dynamic>? ?? const [])
            .map((e) => SendSlotModel.fromJson(e as Map<String, dynamic>))
            .toList(),
        midiClips: (j['midiClips'] as List<dynamic>? ?? const [])
            .map((e) => MidiClipModel.fromJson(e as Map<String, dynamic>))
            .toList(),
        audioClips: (j['audioClips'] as List<dynamic>? ?? const [])
            .map((e) => AudioClipModel.fromJson(e as Map<String, dynamic>))
            .toList(),
      );
}

class ProjectModel {
  final String title;
  final double bpm;
  final int timeSigNum;
  final int timeSigDen;
  final int sampleRate;
  final double playheadBeats;
  final bool isPlaying;
  final bool isRecording;
  final bool loopEnabled;
  final double loopStartBeats;
  final double loopEndBeats;
  final bool metronomeEnabled;
  final bool countInEnabled;
  final bool masterBinauralEnabled;
  final double roomReverbMix;
  final String selectedTrackId;
  final String? selectedClipId;
  final bool canUndo;
  final bool canRedo;
  final List<TrackModel> tracks;
  final TrackModel masterTrack;

  const ProjectModel({
    required this.title,
    required this.bpm,
    required this.timeSigNum,
    required this.timeSigDen,
    required this.sampleRate,
    required this.playheadBeats,
    required this.isPlaying,
    required this.isRecording,
    required this.loopEnabled,
    required this.loopStartBeats,
    required this.loopEndBeats,
    required this.metronomeEnabled,
    required this.countInEnabled,
    required this.masterBinauralEnabled,
    required this.roomReverbMix,
    required this.selectedTrackId,
    required this.selectedClipId,
    required this.canUndo,
    required this.canRedo,
    required this.tracks,
    required this.masterTrack,
  });

  factory ProjectModel.fromJson(Map<String, dynamic> j) => ProjectModel(
        title: j['title'] as String? ?? 'Futureboard Session',
        bpm: (j['bpm'] as num? ?? 124.0).toDouble(),
        timeSigNum: (j['timeSigNum'] as num? ?? 4).toInt(),
        timeSigDen: (j['timeSigDen'] as num? ?? 4).toInt(),
        sampleRate: (j['sampleRate'] as num? ?? 48000).toInt(),
        playheadBeats: (j['playheadBeats'] as num? ?? 0.0).toDouble(),
        isPlaying: j['isPlaying'] as bool? ?? false,
        isRecording: j['isRecording'] as bool? ?? false,
        loopEnabled: j['loopEnabled'] as bool? ?? true,
        loopStartBeats: (j['loopStartBeats'] as num? ?? 0.0).toDouble(),
        loopEndBeats: (j['loopEndBeats'] as num? ?? 16.0).toDouble(),
        metronomeEnabled: j['metronomeEnabled'] as bool? ?? false,
        countInEnabled: j['countInEnabled'] as bool? ?? false,
        masterBinauralEnabled: j['masterBinauralEnabled'] as bool? ?? false,
        roomReverbMix: (j['roomReverbMix'] as num? ?? 0.25).toDouble(),
        selectedTrackId: j['selectedTrackId'] as String? ?? 'track-1',
        selectedClipId: j['selectedClipId'] as String?,
        canUndo: j['canUndo'] as bool? ?? false,
        canRedo: j['canRedo'] as bool? ?? false,
        tracks: (j['tracks'] as List<dynamic>? ?? const [])
            .map((e) => TrackModel.fromJson(e as Map<String, dynamic>))
            .toList(),
        masterTrack: j['masterTrack'] != null
            ? TrackModel.fromJson(j['masterTrack'] as Map<String, dynamic>)
            : TrackModel.fromJson(const {
                'id': 'master',
                'name': 'Master Bus',
                'kind': 'master',
                'colorHex': '#4C8DFF',
              }),
      );
}

class TrackMeterModel {
  final String trackId;
  final double peakLDb;
  final double peakRDb;
  final double rmsLDb;
  final double rmsRDb;
  final double gainReductionDb;
  final bool clipping;

  const TrackMeterModel({
    required this.trackId,
    required this.peakLDb,
    required this.peakRDb,
    required this.rmsLDb,
    required this.rmsRDb,
    required this.gainReductionDb,
    required this.clipping,
  });

  factory TrackMeterModel.fromJson(Map<String, dynamic> j) => TrackMeterModel(
        trackId: j['trackId'] as String? ?? '',
        peakLDb: (j['peakLDb'] as num? ?? -96.0).toDouble(),
        peakRDb: (j['peakRDb'] as num? ?? -96.0).toDouble(),
        rmsLDb: (j['rmsLDb'] as num? ?? -96.0).toDouble(),
        rmsRDb: (j['rmsRDb'] as num? ?? -96.0).toDouble(),
        gainReductionDb: (j['gainReductionDb'] as num? ?? 0.0).toDouble(),
        clipping: j['clipping'] as bool? ?? false,
      );
}

class PluginMeterModel {
  final double inputPeakDb;
  final double outputPeakDb;
  final double gainReductionDb;
  final List<double> padLevels;
  final List<double> eqCurveDb;

  const PluginMeterModel({
    required this.inputPeakDb,
    required this.outputPeakDb,
    required this.gainReductionDb,
    required this.padLevels,
    required this.eqCurveDb,
  });

  factory PluginMeterModel.fromJson(Map<String, dynamic> j) => PluginMeterModel(
        inputPeakDb: (j['inputPeakDb'] as num? ?? -96.0).toDouble(),
        outputPeakDb: (j['outputPeakDb'] as num? ?? -96.0).toDouble(),
        gainReductionDb: (j['gainReductionDb'] as num? ?? 0.0).toDouble(),
        padLevels: (j['padLevels'] as List<dynamic>? ?? const [])
            .map((e) => (e as num).toDouble())
            .toList(),
        eqCurveDb: (j['eqCurveDb'] as List<dynamic>? ?? const [])
            .map((e) => (e as num).toDouble())
            .toList(),
      );
}

class TelemetryModel {
  final double playheadBeats;
  final bool isPlaying;
  final bool isRecording;
  final double bpm;
  final double masterPeakLDb;
  final double masterPeakRDb;
  final double masterRmsLDb;
  final double masterRmsRDb;
  final double lufsMomentary;
  final double lufsShortTerm;
  final double lufsIntegrated;
  final double truePeakDbtp;
  final double cpuLoadPct;
  final List<double> spectrumDb;
  final List<TrackMeterModel> trackMeters;
  final PluginMeterModel? inspectedPluginMeter;

  const TelemetryModel({
    required this.playheadBeats,
    required this.isPlaying,
    required this.isRecording,
    required this.bpm,
    required this.masterPeakLDb,
    required this.masterPeakRDb,
    required this.masterRmsLDb,
    required this.masterRmsRDb,
    required this.lufsMomentary,
    required this.lufsShortTerm,
    required this.lufsIntegrated,
    required this.truePeakDbtp,
    required this.cpuLoadPct,
    required this.spectrumDb,
    required this.trackMeters,
    this.inspectedPluginMeter,
  });

  factory TelemetryModel.empty() => TelemetryModel(
        playheadBeats: 0.0,
        isPlaying: false,
        isRecording: false,
        bpm: 124.0,
        masterPeakLDb: -96.0,
        masterPeakRDb: -96.0,
        masterRmsLDb: -96.0,
        masterRmsRDb: -96.0,
        lufsMomentary: -70.0,
        lufsShortTerm: -70.0,
        lufsIntegrated: -70.0,
        truePeakDbtp: -96.0,
        cpuLoadPct: 4.0,
        spectrumDb: List<double>.filled(128, -90.0),
        trackMeters: const [],
      );

  factory TelemetryModel.fromJson(Map<String, dynamic> j) => TelemetryModel(
        playheadBeats: (j['playheadBeats'] as num? ?? 0.0).toDouble(),
        isPlaying: j['isPlaying'] as bool? ?? false,
        isRecording: j['isRecording'] as bool? ?? false,
        bpm: (j['bpm'] as num? ?? 124.0).toDouble(),
        masterPeakLDb: (j['masterPeakLDb'] as num? ?? -96.0).toDouble(),
        masterPeakRDb: (j['masterPeakRDb'] as num? ?? -96.0).toDouble(),
        masterRmsLDb: (j['masterRmsLDb'] as num? ?? -96.0).toDouble(),
        masterRmsRDb: (j['masterRmsRDb'] as num? ?? -96.0).toDouble(),
        lufsMomentary: (j['lufsMomentary'] as num? ?? -70.0).toDouble(),
        lufsShortTerm: (j['lufsShortTerm'] as num? ?? -70.0).toDouble(),
        lufsIntegrated: (j['lufsIntegrated'] as num? ?? -70.0).toDouble(),
        truePeakDbtp: (j['truePeakDbtp'] as num? ?? -96.0).toDouble(),
        cpuLoadPct: (j['cpuLoadPct'] as num? ?? 4.0).toDouble(),
        spectrumDb: (j['spectrumDb'] as List<dynamic>? ?? const [])
            .map((e) => (e as num).toDouble())
            .toList(),
        trackMeters: (j['trackMeters'] as List<dynamic>? ?? const [])
            .map((e) => TrackMeterModel.fromJson(e as Map<String, dynamic>))
            .toList(),
        inspectedPluginMeter: j['inspectedPluginMeter'] != null
            ? PluginMeterModel.fromJson(
                j['inspectedPluginMeter'] as Map<String, dynamic>)
            : null,
      );

  TrackMeterModel meterForTrack(String trackId) {
    for (final m in trackMeters) {
      if (m.trackId == trackId) return m;
    }
    return TrackMeterModel(
      trackId: trackId,
      peakLDb: -96.0,
      peakRDb: -96.0,
      rmsLDb: -96.0,
      rmsRDb: -96.0,
      gainReductionDb: 0.0,
      clipping: false,
    );
  }
}

class CatalogPluginModel {
  final String id;
  final String name;
  final String category;
  final bool isInstrument;
  final String description;
  final List<String> presets;

  const CatalogPluginModel({
    required this.id,
    required this.name,
    required this.category,
    required this.isInstrument,
    required this.description,
    required this.presets,
  });

  factory CatalogPluginModel.fromJson(Map<String, dynamic> j) =>
      CatalogPluginModel(
        id: j['id'] as String? ?? '',
        name: j['name'] as String? ?? '',
        category: j['category'] as String? ?? 'Effect',
        isInstrument:
            (j['kind'] as String? ?? '').toLowerCase() == 'instrument',
        description: j['description'] as String? ?? '',
        presets: (j['presets'] as List<dynamic>? ?? const [])
            .map((e) => e.toString())
            .toList(),
      );
}
