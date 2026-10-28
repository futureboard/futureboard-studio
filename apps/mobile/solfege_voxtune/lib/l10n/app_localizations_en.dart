// ignore: unused_import
import 'package:intl/intl.dart' as intl;

import 'app_localizations.dart';

// ignore_for_file: type=lint

/// The translations for English (`en`).
class AppLocalizationsEn extends AppLocalizations {
  AppLocalizationsEn([String locale = 'en']) : super(locale);

  @override
  String get noteCut => 'Cut';

  @override
  String get noteJoin => 'Join';

  @override
  String get noteMute => 'Mute';

  @override
  String get noteUnmute => 'Unmute';

  @override
  String get noteReset => 'Reset';

  @override
  String get cutHint => 'Drag across a note, lift to cut';

  @override
  String get done => 'Done';

  @override
  String get noteJoinSemantic => 'Join with the next note';

  @override
  String get muted => 'muted';

  @override
  String get advancedTools => 'Advanced tools';

  @override
  String get advScope => 'Apply to';

  @override
  String get advScopeAll => 'All notes';

  @override
  String get advScopeSelected => 'Selected note';

  @override
  String get advCenter => 'Correct pitch center';

  @override
  String get advCenterHint =>
      'Pulls each note toward the nearest note of the scale';

  @override
  String get advDrift => 'Reduce pitch drift';

  @override
  String get advDriftHint => 'Steadies wobble and vibrato inside notes';

  @override
  String get advQuantize => 'Quantize timing';

  @override
  String get advQuantizeHint => 'Moves note starts toward the beat grid';

  @override
  String get advGrid => 'Grid';

  @override
  String get advStrength => 'Strength';

  @override
  String get advTranspose => 'Transpose';

  @override
  String get advTransposeHint =>
      'Shifts pitch by semitones; formants stay natural';

  @override
  String get advLevel => 'Even out levels';

  @override
  String get advLevelHint => 'Brings loud and quiet notes closer together';

  @override
  String get advAmount => 'Amount';

  @override
  String get apply => 'Apply';

  @override
  String advApplied(int count) {
    String _temp0 = intl.Intl.pluralLogic(
      count,
      locale: localeName,
      other: 'Applied to $count notes',
      one: 'Applied to 1 note',
      zero: 'No notes changed',
    );
    return '$_temp0';
  }

  @override
  String get record => 'Record';

  @override
  String get recordTake => 'Record a take';

  @override
  String get recordTakeHint =>
      'Sing into the microphone — the take opens here when you stop';

  @override
  String get recordTapToStart => 'Tap to start recording';

  @override
  String get recordTapToStop => 'Tap to stop — the take opens in the editor';

  @override
  String get recordSingNote => 'Sing a note';

  @override
  String get recordTooLoud => 'Too loud — move back from the mic';

  @override
  String get recordNeedsMic => 'Microphone access is needed to record.';

  @override
  String get recordAllowMic => 'Allow microphone';

  @override
  String recordFailed(String error) {
    return 'Recording isn’t available: $error';
  }

  @override
  String get recordSaving => 'Saving…';

  @override
  String get discardTakeTitle => 'Discard this take?';

  @override
  String get discardTakeBody => 'What you recorded so far will be deleted.';

  @override
  String get discard => 'Discard';

  @override
  String get keepRecording => 'Keep recording';

  @override
  String takeName(int n) {
    return 'Take $n';
  }

  @override
  String get appName => 'Solfege Voxtune';

  @override
  String get projects => 'Projects';

  @override
  String get newProject => 'New project';

  @override
  String get projectName => 'Project name';

  @override
  String get untitledProject => 'Untitled project';

  @override
  String get create => 'Create';

  @override
  String get cancel => 'Cancel';

  @override
  String get delete => 'Delete';

  @override
  String get remove => 'Remove';

  @override
  String get homeEmptyTitle => 'No projects yet';

  @override
  String get homeEmptyBody =>
      'Create a project, add your vocal takes, and every edit is saved automatically.';

  @override
  String projectFiles(int count) {
    String _temp0 = intl.Intl.pluralLogic(
      count,
      locale: localeName,
      other: '$count files',
      one: '1 file',
      zero: 'No files',
    );
    return '$_temp0';
  }

  @override
  String get deleteProject => 'Delete project';

  @override
  String deleteProjectTitle(String name) {
    return 'Delete “$name”?';
  }

  @override
  String get deleteProjectBody =>
      'The project folder with its audio copies and exports will be removed from this device.';

  @override
  String savedIn(String path) {
    return 'Saved in $path';
  }

  @override
  String storageError(String error) {
    return 'Can’t open the projects folder: $error';
  }

  @override
  String get openMenu => 'Open menu';

  @override
  String get files => 'Files';

  @override
  String get addAudioFile => 'Add audio file';

  @override
  String get notOpenedYet => 'Not opened yet';

  @override
  String get allProjects => 'All projects';

  @override
  String get settings => 'Settings';

  @override
  String get export => 'Export';

  @override
  String get emptyProjectTitle => 'Add a vocal take';

  @override
  String get emptyProjectBody =>
      'This project has no audio yet. Add a dry, monophonic vocal recording — Solfege Voxtune detects every note, the key and the tempo so you can fix pitch and timing by hand.';

  @override
  String get removeFile => 'Remove from project';

  @override
  String get removeFileHint => 'Deletes this take’s copy and its edits';

  @override
  String removeFileTitle(String name) {
    return 'Remove “$name”?';
  }

  @override
  String get removeFileBody =>
      'The audio copy in this project and all its edits will be deleted. The original file on your device is not touched.';

  @override
  String get exportThis => 'Export this file';

  @override
  String get exportThisHint => 'WAV · 48 kHz · mono, with all edits';

  @override
  String get exportAll => 'Export all files';

  @override
  String exportAllHint(int count) {
    String _temp0 = intl.Intl.pluralLogic(
      count,
      locale: localeName,
      other: '$count files',
      one: '1 file',
    );
    return '$_temp0 to the project’s Exports folder';
  }

  @override
  String get exporting => 'Exporting…';

  @override
  String exportDone(String path) {
    return 'Saved to $path';
  }

  @override
  String exportAllDone(int count, String path) {
    String _temp0 = intl.Intl.pluralLogic(
      count,
      locale: localeName,
      other: '$count files exported',
      one: '1 file exported',
    );
    return '$_temp0 to $path';
  }

  @override
  String exportFailed(String error) {
    return 'Export failed: $error';
  }

  @override
  String get defaultSnap => 'Default pitch snap';

  @override
  String get defaultSnapHint => 'Used when a project opens';

  @override
  String get storage => 'Storage';

  @override
  String get storageHint => 'Projects, audio copies and exports are saved here';

  @override
  String get about => 'About';

  @override
  String version(String version) {
    return 'Version $version';
  }

  @override
  String get aboutBody =>
      'Vocal pitch and timing editor. Audio engine in Rust: YIN pitch analysis and TD-PSOLA pitch/formant shifting, rendered on a native audio thread.';

  @override
  String get noAudioLoaded => 'No audio loaded';

  @override
  String get audioClip => 'Audio clip';

  @override
  String get loadFailed => 'Failed to decode or analyze audio file';

  @override
  String get toolPitch => 'Pitch';

  @override
  String get toolDrift => 'Drift';

  @override
  String get toolTime => 'Time';

  @override
  String get toolFormant => 'Formant';

  @override
  String get toolLevel => 'Level';

  @override
  String toolSemantic(String tool) {
    return '$tool tool';
  }

  @override
  String get modeMinor => 'minor';

  @override
  String get modeMajor => 'major';

  @override
  String get modePenta => 'pentatonic';

  @override
  String get modeTitleMinor => 'Minor';

  @override
  String get modeTitleMajor => 'Major';

  @override
  String get modeTitlePenta => 'Pentatonic';

  @override
  String keyChip(String key, String mode) {
    return '$key $mode';
  }

  @override
  String get snapFree => 'Free';

  @override
  String get snapSemi => 'Semi';

  @override
  String get snapScale => 'Scale';

  @override
  String snapSemantic(String mode) {
    return 'Pitch snap: $mode';
  }

  @override
  String get inKey => 'In key';

  @override
  String offCount(int count) {
    return '$count off';
  }

  @override
  String get nextOffSemantic => 'Select next off-key note';

  @override
  String get keySheetTitle => 'Key & scale';

  @override
  String detectedKey(String key, String mode) {
    return 'Detected $key $mode';
  }

  @override
  String get rootLabel => 'Root';

  @override
  String get scaleLabel => 'Scale';

  @override
  String get keySheetNote =>
      'Correction and snapping target this scale. Notes you edited by hand keep their pitch.';

  @override
  String get sessionLength => 'Length';

  @override
  String get sessionTempo => 'Tempo';

  @override
  String get sessionKey => 'Key';

  @override
  String get sessionNotes => 'Notes';

  @override
  String bpmValue(int bpm) {
    return '$bpm BPM';
  }

  @override
  String get openAudioFile => 'Open audio file';

  @override
  String get openAudioFileHint =>
      'WAV, MP3, FLAC, OGG or M4A — mono vocal works best';

  @override
  String get reanalyze => 'Re-analyze';

  @override
  String get reanalyzeHint =>
      'Detect notes, key and tempo again. Clears edits.';

  @override
  String get resetAll => 'Reset all edits';

  @override
  String get resetAllHint =>
      'Return every note to its detected pitch and timing';

  @override
  String get language => 'Language';

  @override
  String get languageSystem => 'System';

  @override
  String get undo => 'Undo';

  @override
  String get redo => 'Redo';

  @override
  String get zoomIn => 'Zoom in';

  @override
  String get zoomOut => 'Zoom out';

  @override
  String get emptyErrorTitle => 'Couldn’t open that file';

  @override
  String get emptyNoNotesTitle => 'No sung notes found';

  @override
  String get emptyTitle => 'Tune a vocal take';

  @override
  String get emptyNoNotesBody =>
      'The clip has no pitched phrases above the noise floor. Try a dry, monophonic vocal.';

  @override
  String get emptyBody =>
      'Open a dry, monophonic vocal recording. Solfege Voxtune detects every note, the key and the tempo so you can fix pitch and timing by hand.';

  @override
  String get analyzing => 'Analyzing…';

  @override
  String get correctLabel => 'Correct';

  @override
  String get tuneAll => 'Tune all';

  @override
  String get snap => 'Snap';

  @override
  String get revert => 'Revert';

  @override
  String get inspBreath => 'Breath / noise';

  @override
  String get inspNoNotes => 'No notes detected';

  @override
  String get inspAllInKey => 'Every note is in key';

  @override
  String inspOffScale(int count) {
    String _temp0 = intl.Intl.pluralLogic(
      count,
      locale: localeName,
      other: '$count notes are off the scale',
      one: '1 note is off the scale',
    );
    return '$_temp0';
  }

  @override
  String get inspUnpitchedHint => 'Unpitched — pitch tools don’t apply';

  @override
  String get inspOpenHint => 'Open a vocal file to start';

  @override
  String get inspTapHint => 'Tap a note to inspect and edit it';

  @override
  String metaBar(String position) {
    return 'bar $position';
  }

  @override
  String metaWas(String cents) {
    return 'was $cents';
  }

  @override
  String get metaEdited => 'edited';

  @override
  String get backToStart => 'Back to start';

  @override
  String get play => 'Play';

  @override
  String get pause => 'Pause';

  @override
  String get loop => 'Loop';

  @override
  String get bypassSemantic => 'Bypass edits';

  @override
  String get bypassOn => 'Original';

  @override
  String get bypassOff => 'Bypass';

  @override
  String get bypassTag => 'BYPASS';

  @override
  String get history => 'History';

  @override
  String get historyHint =>
      'Tap a step to go back to it. History is kept when you close the app.';

  @override
  String get historyEmpty => 'No edits yet';

  @override
  String get historyStart => 'Start';

  @override
  String get historyNow => 'Now';

  @override
  String historyStepsHint(int count) {
    String _temp0 = intl.Intl.pluralLogic(
      count,
      locale: localeName,
      other: '$count steps · go back to any point',
      one: '1 step · go back to any point',
      zero: 'No edits yet',
    );
    return '$_temp0';
  }

  @override
  String undoToast(String action) {
    return 'Undo · $action';
  }

  @override
  String redoToast(String action) {
    return 'Redo · $action';
  }

  @override
  String histKey(String name) {
    return 'Key $name';
  }

  @override
  String histScale(String name) {
    return 'Scale $name';
  }

  @override
  String get histSnapNote => 'Snap note';

  @override
  String get histCorrect => 'Correction amount';

  @override
  String get histCut => 'Cut note';

  @override
  String get histJoin => 'Join notes';

  @override
  String get histMute => 'Mute note';

  @override
  String get histUnmute => 'Unmute note';

  @override
  String get histReset => 'Reset note';

  @override
  String get histEdit => 'Edit';

  @override
  String get cache => 'Cache';

  @override
  String get cacheHint =>
      'Saved analysis and undo history, so files open instantly. Clearing it keeps all your edits: files are analysed again and undo history starts over.';

  @override
  String cacheUsed(String size) {
    return '$size used';
  }

  @override
  String get clearCache => 'Clear cache';

  @override
  String get cacheCleared => 'Cache cleared';

  @override
  String get loadingProject => 'Loading project';

  @override
  String get loadingFile => 'Opening file';

  @override
  String get reanalyzingFile => 'Analysing again';

  @override
  String get loadStageDecode => 'Reading audio…';

  @override
  String get loadStageAnalyze => 'Analysing pitch and notes';

  @override
  String get loadStageRestore => 'Restoring your edits…';

  @override
  String get loadHintCached => 'Using the saved analysis, so this is quick.';

  @override
  String get loadHintFirst =>
      'First time this file is opened, so it is analysed. Next time it opens instantly.';

  @override
  String get cancelling => 'Cancelling…';

  @override
  String get notOpenedBody =>
      'Opening was cancelled. Open it to analyse and edit it.';

  @override
  String get openFile => 'Open file';

  @override
  String get noteExpression => 'Ornament';

  @override
  String get expressionTitle => 'Ornament & accent';

  @override
  String get listen => 'Listen';

  @override
  String get ornament => 'Ornament (luk khor)';

  @override
  String get ornNone => 'None';

  @override
  String get ornTurnUp => 'Flick up';

  @override
  String get ornTurnDown => 'Flick down';

  @override
  String get ornShake => 'Shake';

  @override
  String get ornFall => 'Fall';

  @override
  String get ornDepth => 'Width';

  @override
  String get ornSpeed => 'Speed';

  @override
  String get accent => 'Accent';

  @override
  String get accentHint =>
      'Punches the start of the note, up to +6 dB, then settles back.';

  @override
  String get accentAmount => 'Strength';

  @override
  String get clearExpression => 'Clear';

  @override
  String get histOrnament => 'Ornament';

  @override
  String get histAccent => 'Accent';

  @override
  String get histExprClear => 'Clear ornament';

  @override
  String metaAccent(int percent) {
    return 'accent $percent%';
  }

  @override
  String get advAccentBeats => 'Accent on the beat';

  @override
  String get advAccentBeatsHint =>
      'Accents notes that start on a beat, for a tighter groove';
}
