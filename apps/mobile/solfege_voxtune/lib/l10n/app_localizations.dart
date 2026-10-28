import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_localizations/flutter_localizations.dart';
import 'package:intl/intl.dart' as intl;

import 'app_localizations_en.dart';
import 'app_localizations_th.dart';

// ignore_for_file: type=lint

/// Callers can lookup localized strings with an instance of AppLocalizations
/// returned by `AppLocalizations.of(context)`.
///
/// Applications need to include `AppLocalizations.delegate()` in their app's
/// `localizationDelegates` list, and the locales they support in the app's
/// `supportedLocales` list. For example:
///
/// ```dart
/// import 'l10n/app_localizations.dart';
///
/// return MaterialApp(
///   localizationsDelegates: AppLocalizations.localizationsDelegates,
///   supportedLocales: AppLocalizations.supportedLocales,
///   home: MyApplicationHome(),
/// );
/// ```
///
/// ## Update pubspec.yaml
///
/// Please make sure to update your pubspec.yaml to include the following
/// packages:
///
/// ```yaml
/// dependencies:
///   # Internationalization support.
///   flutter_localizations:
///     sdk: flutter
///   intl: any # Use the pinned version from flutter_localizations
///
///   # Rest of dependencies
/// ```
///
/// ## iOS Applications
///
/// iOS applications define key application metadata, including supported
/// locales, in an Info.plist file that is built into the application bundle.
/// To configure the locales supported by your app, you’ll need to edit this
/// file.
///
/// First, open your project’s ios/Runner.xcworkspace Xcode workspace file.
/// Then, in the Project Navigator, open the Info.plist file under the Runner
/// project’s Runner folder.
///
/// Next, select the Information Property List item, select Add Item from the
/// Editor menu, then select Localizations from the pop-up menu.
///
/// Select and expand the newly-created Localizations item then, for each
/// locale your application supports, add a new item and select the locale
/// you wish to add from the pop-up menu in the Value field. This list should
/// be consistent with the languages listed in the AppLocalizations.supportedLocales
/// property.
abstract class AppLocalizations {
  AppLocalizations(String locale)
    : localeName = intl.Intl.canonicalizedLocale(locale.toString());

  final String localeName;

  static AppLocalizations of(BuildContext context) {
    return Localizations.of<AppLocalizations>(context, AppLocalizations)!;
  }

  static const LocalizationsDelegate<AppLocalizations> delegate =
      _AppLocalizationsDelegate();

  /// A list of this localizations delegate along with the default localizations
  /// delegates.
  ///
  /// Returns a list of localizations delegates containing this delegate along with
  /// GlobalMaterialLocalizations.delegate, GlobalCupertinoLocalizations.delegate,
  /// and GlobalWidgetsLocalizations.delegate.
  ///
  /// Additional delegates can be added by appending to this list in
  /// MaterialApp. This list does not have to be used at all if a custom list
  /// of delegates is preferred or required.
  static const List<LocalizationsDelegate<dynamic>> localizationsDelegates =
      <LocalizationsDelegate<dynamic>>[
        delegate,
        GlobalMaterialLocalizations.delegate,
        GlobalCupertinoLocalizations.delegate,
        GlobalWidgetsLocalizations.delegate,
      ];

  /// A list of this localizations delegate's supported locales.
  static const List<Locale> supportedLocales = <Locale>[
    Locale('en'),
    Locale('th'),
  ];

  /// No description provided for @noteCut.
  ///
  /// In en, this message translates to:
  /// **'Cut'**
  String get noteCut;

  /// No description provided for @noteJoin.
  ///
  /// In en, this message translates to:
  /// **'Join'**
  String get noteJoin;

  /// No description provided for @noteMute.
  ///
  /// In en, this message translates to:
  /// **'Mute'**
  String get noteMute;

  /// No description provided for @noteUnmute.
  ///
  /// In en, this message translates to:
  /// **'Unmute'**
  String get noteUnmute;

  /// No description provided for @noteReset.
  ///
  /// In en, this message translates to:
  /// **'Reset'**
  String get noteReset;

  /// No description provided for @cutHint.
  ///
  /// In en, this message translates to:
  /// **'Drag across a note, lift to cut'**
  String get cutHint;

  /// No description provided for @done.
  ///
  /// In en, this message translates to:
  /// **'Done'**
  String get done;

  /// No description provided for @noteJoinSemantic.
  ///
  /// In en, this message translates to:
  /// **'Join with the next note'**
  String get noteJoinSemantic;

  /// No description provided for @muted.
  ///
  /// In en, this message translates to:
  /// **'muted'**
  String get muted;

  /// No description provided for @advancedTools.
  ///
  /// In en, this message translates to:
  /// **'Advanced tools'**
  String get advancedTools;

  /// No description provided for @advScope.
  ///
  /// In en, this message translates to:
  /// **'Apply to'**
  String get advScope;

  /// No description provided for @advScopeAll.
  ///
  /// In en, this message translates to:
  /// **'All notes'**
  String get advScopeAll;

  /// No description provided for @advScopeSelected.
  ///
  /// In en, this message translates to:
  /// **'Selected note'**
  String get advScopeSelected;

  /// No description provided for @advCenter.
  ///
  /// In en, this message translates to:
  /// **'Correct pitch center'**
  String get advCenter;

  /// No description provided for @advCenterHint.
  ///
  /// In en, this message translates to:
  /// **'Pulls each note toward the nearest note of the scale'**
  String get advCenterHint;

  /// No description provided for @advDrift.
  ///
  /// In en, this message translates to:
  /// **'Reduce pitch drift'**
  String get advDrift;

  /// No description provided for @advDriftHint.
  ///
  /// In en, this message translates to:
  /// **'Steadies wobble and vibrato inside notes'**
  String get advDriftHint;

  /// No description provided for @advQuantize.
  ///
  /// In en, this message translates to:
  /// **'Quantize timing'**
  String get advQuantize;

  /// No description provided for @advQuantizeHint.
  ///
  /// In en, this message translates to:
  /// **'Moves note starts toward the beat grid'**
  String get advQuantizeHint;

  /// No description provided for @advGrid.
  ///
  /// In en, this message translates to:
  /// **'Grid'**
  String get advGrid;

  /// No description provided for @advStrength.
  ///
  /// In en, this message translates to:
  /// **'Strength'**
  String get advStrength;

  /// No description provided for @advTranspose.
  ///
  /// In en, this message translates to:
  /// **'Transpose'**
  String get advTranspose;

  /// No description provided for @advTransposeHint.
  ///
  /// In en, this message translates to:
  /// **'Shifts pitch by semitones; formants stay natural'**
  String get advTransposeHint;

  /// No description provided for @advLevel.
  ///
  /// In en, this message translates to:
  /// **'Even out levels'**
  String get advLevel;

  /// No description provided for @advLevelHint.
  ///
  /// In en, this message translates to:
  /// **'Brings loud and quiet notes closer together'**
  String get advLevelHint;

  /// No description provided for @advAmount.
  ///
  /// In en, this message translates to:
  /// **'Amount'**
  String get advAmount;

  /// No description provided for @apply.
  ///
  /// In en, this message translates to:
  /// **'Apply'**
  String get apply;

  /// No description provided for @advApplied.
  ///
  /// In en, this message translates to:
  /// **'{count, plural, =0{No notes changed} =1{Applied to 1 note} other{Applied to {count} notes}}'**
  String advApplied(int count);

  /// No description provided for @record.
  ///
  /// In en, this message translates to:
  /// **'Record'**
  String get record;

  /// No description provided for @recordTake.
  ///
  /// In en, this message translates to:
  /// **'Record a take'**
  String get recordTake;

  /// No description provided for @recordTakeHint.
  ///
  /// In en, this message translates to:
  /// **'Sing into the microphone — the take opens here when you stop'**
  String get recordTakeHint;

  /// No description provided for @recordTapToStart.
  ///
  /// In en, this message translates to:
  /// **'Tap to start recording'**
  String get recordTapToStart;

  /// No description provided for @recordTapToStop.
  ///
  /// In en, this message translates to:
  /// **'Tap to stop — the take opens in the editor'**
  String get recordTapToStop;

  /// No description provided for @recordSingNote.
  ///
  /// In en, this message translates to:
  /// **'Sing a note'**
  String get recordSingNote;

  /// No description provided for @recordTooLoud.
  ///
  /// In en, this message translates to:
  /// **'Too loud — move back from the mic'**
  String get recordTooLoud;

  /// No description provided for @recordNeedsMic.
  ///
  /// In en, this message translates to:
  /// **'Microphone access is needed to record.'**
  String get recordNeedsMic;

  /// No description provided for @recordAllowMic.
  ///
  /// In en, this message translates to:
  /// **'Allow microphone'**
  String get recordAllowMic;

  /// No description provided for @recordFailed.
  ///
  /// In en, this message translates to:
  /// **'Recording isn’t available: {error}'**
  String recordFailed(String error);

  /// No description provided for @recordSaving.
  ///
  /// In en, this message translates to:
  /// **'Saving…'**
  String get recordSaving;

  /// No description provided for @discardTakeTitle.
  ///
  /// In en, this message translates to:
  /// **'Discard this take?'**
  String get discardTakeTitle;

  /// No description provided for @discardTakeBody.
  ///
  /// In en, this message translates to:
  /// **'What you recorded so far will be deleted.'**
  String get discardTakeBody;

  /// No description provided for @discard.
  ///
  /// In en, this message translates to:
  /// **'Discard'**
  String get discard;

  /// No description provided for @keepRecording.
  ///
  /// In en, this message translates to:
  /// **'Keep recording'**
  String get keepRecording;

  /// No description provided for @takeName.
  ///
  /// In en, this message translates to:
  /// **'Take {n}'**
  String takeName(int n);

  /// No description provided for @appName.
  ///
  /// In en, this message translates to:
  /// **'Solfege Voxtune'**
  String get appName;

  /// No description provided for @projects.
  ///
  /// In en, this message translates to:
  /// **'Projects'**
  String get projects;

  /// No description provided for @newProject.
  ///
  /// In en, this message translates to:
  /// **'New project'**
  String get newProject;

  /// No description provided for @projectName.
  ///
  /// In en, this message translates to:
  /// **'Project name'**
  String get projectName;

  /// No description provided for @untitledProject.
  ///
  /// In en, this message translates to:
  /// **'Untitled project'**
  String get untitledProject;

  /// No description provided for @create.
  ///
  /// In en, this message translates to:
  /// **'Create'**
  String get create;

  /// No description provided for @cancel.
  ///
  /// In en, this message translates to:
  /// **'Cancel'**
  String get cancel;

  /// No description provided for @delete.
  ///
  /// In en, this message translates to:
  /// **'Delete'**
  String get delete;

  /// No description provided for @remove.
  ///
  /// In en, this message translates to:
  /// **'Remove'**
  String get remove;

  /// No description provided for @homeEmptyTitle.
  ///
  /// In en, this message translates to:
  /// **'No projects yet'**
  String get homeEmptyTitle;

  /// No description provided for @homeEmptyBody.
  ///
  /// In en, this message translates to:
  /// **'Create a project, add your vocal takes, and every edit is saved automatically.'**
  String get homeEmptyBody;

  /// No description provided for @projectFiles.
  ///
  /// In en, this message translates to:
  /// **'{count, plural, =0{No files} =1{1 file} other{{count} files}}'**
  String projectFiles(int count);

  /// No description provided for @deleteProject.
  ///
  /// In en, this message translates to:
  /// **'Delete project'**
  String get deleteProject;

  /// No description provided for @deleteProjectTitle.
  ///
  /// In en, this message translates to:
  /// **'Delete “{name}”?'**
  String deleteProjectTitle(String name);

  /// No description provided for @deleteProjectBody.
  ///
  /// In en, this message translates to:
  /// **'The project folder with its audio copies and exports will be removed from this device.'**
  String get deleteProjectBody;

  /// No description provided for @savedIn.
  ///
  /// In en, this message translates to:
  /// **'Saved in {path}'**
  String savedIn(String path);

  /// No description provided for @storageError.
  ///
  /// In en, this message translates to:
  /// **'Can’t open the projects folder: {error}'**
  String storageError(String error);

  /// No description provided for @openMenu.
  ///
  /// In en, this message translates to:
  /// **'Open menu'**
  String get openMenu;

  /// No description provided for @files.
  ///
  /// In en, this message translates to:
  /// **'Files'**
  String get files;

  /// No description provided for @addAudioFile.
  ///
  /// In en, this message translates to:
  /// **'Add audio file'**
  String get addAudioFile;

  /// No description provided for @notOpenedYet.
  ///
  /// In en, this message translates to:
  /// **'Not opened yet'**
  String get notOpenedYet;

  /// No description provided for @allProjects.
  ///
  /// In en, this message translates to:
  /// **'All projects'**
  String get allProjects;

  /// No description provided for @settings.
  ///
  /// In en, this message translates to:
  /// **'Settings'**
  String get settings;

  /// No description provided for @export.
  ///
  /// In en, this message translates to:
  /// **'Export'**
  String get export;

  /// No description provided for @emptyProjectTitle.
  ///
  /// In en, this message translates to:
  /// **'Add a vocal take'**
  String get emptyProjectTitle;

  /// No description provided for @emptyProjectBody.
  ///
  /// In en, this message translates to:
  /// **'This project has no audio yet. Add a dry, monophonic vocal recording — Solfege Voxtune detects every note, the key and the tempo so you can fix pitch and timing by hand.'**
  String get emptyProjectBody;

  /// No description provided for @removeFile.
  ///
  /// In en, this message translates to:
  /// **'Remove from project'**
  String get removeFile;

  /// No description provided for @removeFileHint.
  ///
  /// In en, this message translates to:
  /// **'Deletes this take’s copy and its edits'**
  String get removeFileHint;

  /// No description provided for @removeFileTitle.
  ///
  /// In en, this message translates to:
  /// **'Remove “{name}”?'**
  String removeFileTitle(String name);

  /// No description provided for @removeFileBody.
  ///
  /// In en, this message translates to:
  /// **'The audio copy in this project and all its edits will be deleted. The original file on your device is not touched.'**
  String get removeFileBody;

  /// No description provided for @exportThis.
  ///
  /// In en, this message translates to:
  /// **'Export this file'**
  String get exportThis;

  /// No description provided for @exportThisHint.
  ///
  /// In en, this message translates to:
  /// **'WAV · 48 kHz · mono, with all edits'**
  String get exportThisHint;

  /// No description provided for @exportAll.
  ///
  /// In en, this message translates to:
  /// **'Export all files'**
  String get exportAll;

  /// No description provided for @exportAllHint.
  ///
  /// In en, this message translates to:
  /// **'{count, plural, =1{1 file} other{{count} files}} to the project’s Exports folder'**
  String exportAllHint(int count);

  /// No description provided for @exporting.
  ///
  /// In en, this message translates to:
  /// **'Exporting…'**
  String get exporting;

  /// No description provided for @exportDone.
  ///
  /// In en, this message translates to:
  /// **'Saved to {path}'**
  String exportDone(String path);

  /// No description provided for @exportAllDone.
  ///
  /// In en, this message translates to:
  /// **'{count, plural, =1{1 file exported} other{{count} files exported}} to {path}'**
  String exportAllDone(int count, String path);

  /// No description provided for @exportFailed.
  ///
  /// In en, this message translates to:
  /// **'Export failed: {error}'**
  String exportFailed(String error);

  /// No description provided for @defaultSnap.
  ///
  /// In en, this message translates to:
  /// **'Default pitch snap'**
  String get defaultSnap;

  /// No description provided for @defaultSnapHint.
  ///
  /// In en, this message translates to:
  /// **'Used when a project opens'**
  String get defaultSnapHint;

  /// No description provided for @storage.
  ///
  /// In en, this message translates to:
  /// **'Storage'**
  String get storage;

  /// No description provided for @storageHint.
  ///
  /// In en, this message translates to:
  /// **'Projects, audio copies and exports are saved here'**
  String get storageHint;

  /// No description provided for @about.
  ///
  /// In en, this message translates to:
  /// **'About'**
  String get about;

  /// No description provided for @version.
  ///
  /// In en, this message translates to:
  /// **'Version {version}'**
  String version(String version);

  /// No description provided for @aboutBody.
  ///
  /// In en, this message translates to:
  /// **'Vocal pitch and timing editor. Audio engine in Rust: YIN pitch analysis and TD-PSOLA pitch/formant shifting, rendered on a native audio thread.'**
  String get aboutBody;

  /// No description provided for @noAudioLoaded.
  ///
  /// In en, this message translates to:
  /// **'No audio loaded'**
  String get noAudioLoaded;

  /// No description provided for @audioClip.
  ///
  /// In en, this message translates to:
  /// **'Audio clip'**
  String get audioClip;

  /// No description provided for @loadFailed.
  ///
  /// In en, this message translates to:
  /// **'Failed to decode or analyze audio file'**
  String get loadFailed;

  /// No description provided for @toolPitch.
  ///
  /// In en, this message translates to:
  /// **'Pitch'**
  String get toolPitch;

  /// No description provided for @toolDrift.
  ///
  /// In en, this message translates to:
  /// **'Drift'**
  String get toolDrift;

  /// No description provided for @toolTime.
  ///
  /// In en, this message translates to:
  /// **'Time'**
  String get toolTime;

  /// No description provided for @toolFormant.
  ///
  /// In en, this message translates to:
  /// **'Formant'**
  String get toolFormant;

  /// No description provided for @toolLevel.
  ///
  /// In en, this message translates to:
  /// **'Level'**
  String get toolLevel;

  /// No description provided for @toolSemantic.
  ///
  /// In en, this message translates to:
  /// **'{tool} tool'**
  String toolSemantic(String tool);

  /// No description provided for @modeMinor.
  ///
  /// In en, this message translates to:
  /// **'minor'**
  String get modeMinor;

  /// No description provided for @modeMajor.
  ///
  /// In en, this message translates to:
  /// **'major'**
  String get modeMajor;

  /// No description provided for @modePenta.
  ///
  /// In en, this message translates to:
  /// **'pentatonic'**
  String get modePenta;

  /// No description provided for @modeTitleMinor.
  ///
  /// In en, this message translates to:
  /// **'Minor'**
  String get modeTitleMinor;

  /// No description provided for @modeTitleMajor.
  ///
  /// In en, this message translates to:
  /// **'Major'**
  String get modeTitleMajor;

  /// No description provided for @modeTitlePenta.
  ///
  /// In en, this message translates to:
  /// **'Pentatonic'**
  String get modeTitlePenta;

  /// No description provided for @keyChip.
  ///
  /// In en, this message translates to:
  /// **'{key} {mode}'**
  String keyChip(String key, String mode);

  /// No description provided for @snapFree.
  ///
  /// In en, this message translates to:
  /// **'Free'**
  String get snapFree;

  /// No description provided for @snapSemi.
  ///
  /// In en, this message translates to:
  /// **'Semi'**
  String get snapSemi;

  /// No description provided for @snapScale.
  ///
  /// In en, this message translates to:
  /// **'Scale'**
  String get snapScale;

  /// No description provided for @snapSemantic.
  ///
  /// In en, this message translates to:
  /// **'Pitch snap: {mode}'**
  String snapSemantic(String mode);

  /// No description provided for @inKey.
  ///
  /// In en, this message translates to:
  /// **'In key'**
  String get inKey;

  /// No description provided for @offCount.
  ///
  /// In en, this message translates to:
  /// **'{count} off'**
  String offCount(int count);

  /// No description provided for @nextOffSemantic.
  ///
  /// In en, this message translates to:
  /// **'Select next off-key note'**
  String get nextOffSemantic;

  /// No description provided for @keySheetTitle.
  ///
  /// In en, this message translates to:
  /// **'Key & scale'**
  String get keySheetTitle;

  /// No description provided for @detectedKey.
  ///
  /// In en, this message translates to:
  /// **'Detected {key} {mode}'**
  String detectedKey(String key, String mode);

  /// No description provided for @rootLabel.
  ///
  /// In en, this message translates to:
  /// **'Root'**
  String get rootLabel;

  /// No description provided for @scaleLabel.
  ///
  /// In en, this message translates to:
  /// **'Scale'**
  String get scaleLabel;

  /// No description provided for @keySheetNote.
  ///
  /// In en, this message translates to:
  /// **'Correction and snapping target this scale. Notes you edited by hand keep their pitch.'**
  String get keySheetNote;

  /// No description provided for @sessionLength.
  ///
  /// In en, this message translates to:
  /// **'Length'**
  String get sessionLength;

  /// No description provided for @sessionTempo.
  ///
  /// In en, this message translates to:
  /// **'Tempo'**
  String get sessionTempo;

  /// No description provided for @sessionKey.
  ///
  /// In en, this message translates to:
  /// **'Key'**
  String get sessionKey;

  /// No description provided for @sessionNotes.
  ///
  /// In en, this message translates to:
  /// **'Notes'**
  String get sessionNotes;

  /// No description provided for @bpmValue.
  ///
  /// In en, this message translates to:
  /// **'{bpm} BPM'**
  String bpmValue(int bpm);

  /// No description provided for @openAudioFile.
  ///
  /// In en, this message translates to:
  /// **'Open audio file'**
  String get openAudioFile;

  /// No description provided for @openAudioFileHint.
  ///
  /// In en, this message translates to:
  /// **'WAV, MP3, FLAC, OGG or M4A — mono vocal works best'**
  String get openAudioFileHint;

  /// No description provided for @reanalyze.
  ///
  /// In en, this message translates to:
  /// **'Re-analyze'**
  String get reanalyze;

  /// No description provided for @reanalyzeHint.
  ///
  /// In en, this message translates to:
  /// **'Detect notes, key and tempo again. Clears edits.'**
  String get reanalyzeHint;

  /// No description provided for @resetAll.
  ///
  /// In en, this message translates to:
  /// **'Reset all edits'**
  String get resetAll;

  /// No description provided for @resetAllHint.
  ///
  /// In en, this message translates to:
  /// **'Return every note to its detected pitch and timing'**
  String get resetAllHint;

  /// No description provided for @language.
  ///
  /// In en, this message translates to:
  /// **'Language'**
  String get language;

  /// No description provided for @languageSystem.
  ///
  /// In en, this message translates to:
  /// **'System'**
  String get languageSystem;

  /// No description provided for @undo.
  ///
  /// In en, this message translates to:
  /// **'Undo'**
  String get undo;

  /// No description provided for @redo.
  ///
  /// In en, this message translates to:
  /// **'Redo'**
  String get redo;

  /// No description provided for @zoomIn.
  ///
  /// In en, this message translates to:
  /// **'Zoom in'**
  String get zoomIn;

  /// No description provided for @zoomOut.
  ///
  /// In en, this message translates to:
  /// **'Zoom out'**
  String get zoomOut;

  /// No description provided for @emptyErrorTitle.
  ///
  /// In en, this message translates to:
  /// **'Couldn’t open that file'**
  String get emptyErrorTitle;

  /// No description provided for @emptyNoNotesTitle.
  ///
  /// In en, this message translates to:
  /// **'No sung notes found'**
  String get emptyNoNotesTitle;

  /// No description provided for @emptyTitle.
  ///
  /// In en, this message translates to:
  /// **'Tune a vocal take'**
  String get emptyTitle;

  /// No description provided for @emptyNoNotesBody.
  ///
  /// In en, this message translates to:
  /// **'The clip has no pitched phrases above the noise floor. Try a dry, monophonic vocal.'**
  String get emptyNoNotesBody;

  /// No description provided for @emptyBody.
  ///
  /// In en, this message translates to:
  /// **'Open a dry, monophonic vocal recording. Solfege Voxtune detects every note, the key and the tempo so you can fix pitch and timing by hand.'**
  String get emptyBody;

  /// No description provided for @analyzing.
  ///
  /// In en, this message translates to:
  /// **'Analyzing…'**
  String get analyzing;

  /// No description provided for @correctLabel.
  ///
  /// In en, this message translates to:
  /// **'Correct'**
  String get correctLabel;

  /// No description provided for @tuneAll.
  ///
  /// In en, this message translates to:
  /// **'Tune all'**
  String get tuneAll;

  /// No description provided for @snap.
  ///
  /// In en, this message translates to:
  /// **'Snap'**
  String get snap;

  /// No description provided for @revert.
  ///
  /// In en, this message translates to:
  /// **'Revert'**
  String get revert;

  /// No description provided for @inspBreath.
  ///
  /// In en, this message translates to:
  /// **'Breath / noise'**
  String get inspBreath;

  /// No description provided for @inspNoNotes.
  ///
  /// In en, this message translates to:
  /// **'No notes detected'**
  String get inspNoNotes;

  /// No description provided for @inspAllInKey.
  ///
  /// In en, this message translates to:
  /// **'Every note is in key'**
  String get inspAllInKey;

  /// No description provided for @inspOffScale.
  ///
  /// In en, this message translates to:
  /// **'{count, plural, =1{1 note is off the scale} other{{count} notes are off the scale}}'**
  String inspOffScale(int count);

  /// No description provided for @inspUnpitchedHint.
  ///
  /// In en, this message translates to:
  /// **'Unpitched — pitch tools don’t apply'**
  String get inspUnpitchedHint;

  /// No description provided for @inspOpenHint.
  ///
  /// In en, this message translates to:
  /// **'Open a vocal file to start'**
  String get inspOpenHint;

  /// No description provided for @inspTapHint.
  ///
  /// In en, this message translates to:
  /// **'Tap a note to inspect and edit it'**
  String get inspTapHint;

  /// No description provided for @metaBar.
  ///
  /// In en, this message translates to:
  /// **'bar {position}'**
  String metaBar(String position);

  /// No description provided for @metaWas.
  ///
  /// In en, this message translates to:
  /// **'was {cents}'**
  String metaWas(String cents);

  /// No description provided for @metaEdited.
  ///
  /// In en, this message translates to:
  /// **'edited'**
  String get metaEdited;

  /// No description provided for @backToStart.
  ///
  /// In en, this message translates to:
  /// **'Back to start'**
  String get backToStart;

  /// No description provided for @play.
  ///
  /// In en, this message translates to:
  /// **'Play'**
  String get play;

  /// No description provided for @pause.
  ///
  /// In en, this message translates to:
  /// **'Pause'**
  String get pause;

  /// No description provided for @loop.
  ///
  /// In en, this message translates to:
  /// **'Loop'**
  String get loop;

  /// No description provided for @bypassSemantic.
  ///
  /// In en, this message translates to:
  /// **'Bypass edits'**
  String get bypassSemantic;

  /// No description provided for @bypassOn.
  ///
  /// In en, this message translates to:
  /// **'Original'**
  String get bypassOn;

  /// No description provided for @bypassOff.
  ///
  /// In en, this message translates to:
  /// **'Bypass'**
  String get bypassOff;

  /// No description provided for @bypassTag.
  ///
  /// In en, this message translates to:
  /// **'BYPASS'**
  String get bypassTag;

  /// No description provided for @history.
  ///
  /// In en, this message translates to:
  /// **'History'**
  String get history;

  /// No description provided for @historyHint.
  ///
  /// In en, this message translates to:
  /// **'Tap a step to go back to it. History is kept when you close the app.'**
  String get historyHint;

  /// No description provided for @historyEmpty.
  ///
  /// In en, this message translates to:
  /// **'No edits yet'**
  String get historyEmpty;

  /// No description provided for @historyStart.
  ///
  /// In en, this message translates to:
  /// **'Start'**
  String get historyStart;

  /// No description provided for @historyNow.
  ///
  /// In en, this message translates to:
  /// **'Now'**
  String get historyNow;

  /// No description provided for @historyStepsHint.
  ///
  /// In en, this message translates to:
  /// **'{count, plural, =0{No edits yet} =1{1 step · go back to any point} other{{count} steps · go back to any point}}'**
  String historyStepsHint(int count);

  /// No description provided for @undoToast.
  ///
  /// In en, this message translates to:
  /// **'Undo · {action}'**
  String undoToast(String action);

  /// No description provided for @redoToast.
  ///
  /// In en, this message translates to:
  /// **'Redo · {action}'**
  String redoToast(String action);

  /// No description provided for @histKey.
  ///
  /// In en, this message translates to:
  /// **'Key {name}'**
  String histKey(String name);

  /// No description provided for @histScale.
  ///
  /// In en, this message translates to:
  /// **'Scale {name}'**
  String histScale(String name);

  /// No description provided for @histSnapNote.
  ///
  /// In en, this message translates to:
  /// **'Snap note'**
  String get histSnapNote;

  /// No description provided for @histCorrect.
  ///
  /// In en, this message translates to:
  /// **'Correction amount'**
  String get histCorrect;

  /// No description provided for @histCut.
  ///
  /// In en, this message translates to:
  /// **'Cut note'**
  String get histCut;

  /// No description provided for @histJoin.
  ///
  /// In en, this message translates to:
  /// **'Join notes'**
  String get histJoin;

  /// No description provided for @histMute.
  ///
  /// In en, this message translates to:
  /// **'Mute note'**
  String get histMute;

  /// No description provided for @histUnmute.
  ///
  /// In en, this message translates to:
  /// **'Unmute note'**
  String get histUnmute;

  /// No description provided for @histReset.
  ///
  /// In en, this message translates to:
  /// **'Reset note'**
  String get histReset;

  /// No description provided for @histEdit.
  ///
  /// In en, this message translates to:
  /// **'Edit'**
  String get histEdit;

  /// No description provided for @cache.
  ///
  /// In en, this message translates to:
  /// **'Cache'**
  String get cache;

  /// No description provided for @cacheHint.
  ///
  /// In en, this message translates to:
  /// **'Saved analysis and undo history, so files open instantly. Clearing it keeps all your edits: files are analysed again and undo history starts over.'**
  String get cacheHint;

  /// No description provided for @cacheUsed.
  ///
  /// In en, this message translates to:
  /// **'{size} used'**
  String cacheUsed(String size);

  /// No description provided for @clearCache.
  ///
  /// In en, this message translates to:
  /// **'Clear cache'**
  String get clearCache;

  /// No description provided for @cacheCleared.
  ///
  /// In en, this message translates to:
  /// **'Cache cleared'**
  String get cacheCleared;

  /// No description provided for @loadingProject.
  ///
  /// In en, this message translates to:
  /// **'Loading project'**
  String get loadingProject;

  /// No description provided for @loadingFile.
  ///
  /// In en, this message translates to:
  /// **'Opening file'**
  String get loadingFile;

  /// No description provided for @reanalyzingFile.
  ///
  /// In en, this message translates to:
  /// **'Analysing again'**
  String get reanalyzingFile;

  /// No description provided for @loadStageDecode.
  ///
  /// In en, this message translates to:
  /// **'Reading audio…'**
  String get loadStageDecode;

  /// No description provided for @loadStageAnalyze.
  ///
  /// In en, this message translates to:
  /// **'Analysing pitch and notes'**
  String get loadStageAnalyze;

  /// No description provided for @loadStageRestore.
  ///
  /// In en, this message translates to:
  /// **'Restoring your edits…'**
  String get loadStageRestore;

  /// No description provided for @loadHintCached.
  ///
  /// In en, this message translates to:
  /// **'Using the saved analysis, so this is quick.'**
  String get loadHintCached;

  /// No description provided for @loadHintFirst.
  ///
  /// In en, this message translates to:
  /// **'First time this file is opened, so it is analysed. Next time it opens instantly.'**
  String get loadHintFirst;

  /// No description provided for @cancelling.
  ///
  /// In en, this message translates to:
  /// **'Cancelling…'**
  String get cancelling;

  /// No description provided for @notOpenedBody.
  ///
  /// In en, this message translates to:
  /// **'Opening was cancelled. Open it to analyse and edit it.'**
  String get notOpenedBody;

  /// No description provided for @openFile.
  ///
  /// In en, this message translates to:
  /// **'Open file'**
  String get openFile;

  /// No description provided for @noteExpression.
  ///
  /// In en, this message translates to:
  /// **'Ornament'**
  String get noteExpression;

  /// No description provided for @expressionTitle.
  ///
  /// In en, this message translates to:
  /// **'Ornament & accent'**
  String get expressionTitle;

  /// No description provided for @listen.
  ///
  /// In en, this message translates to:
  /// **'Listen'**
  String get listen;

  /// No description provided for @ornament.
  ///
  /// In en, this message translates to:
  /// **'Ornament (luk khor)'**
  String get ornament;

  /// No description provided for @ornNone.
  ///
  /// In en, this message translates to:
  /// **'None'**
  String get ornNone;

  /// No description provided for @ornTurnUp.
  ///
  /// In en, this message translates to:
  /// **'Flick up'**
  String get ornTurnUp;

  /// No description provided for @ornTurnDown.
  ///
  /// In en, this message translates to:
  /// **'Flick down'**
  String get ornTurnDown;

  /// No description provided for @ornShake.
  ///
  /// In en, this message translates to:
  /// **'Shake'**
  String get ornShake;

  /// No description provided for @ornFall.
  ///
  /// In en, this message translates to:
  /// **'Fall'**
  String get ornFall;

  /// No description provided for @ornDepth.
  ///
  /// In en, this message translates to:
  /// **'Width'**
  String get ornDepth;

  /// No description provided for @ornSpeed.
  ///
  /// In en, this message translates to:
  /// **'Speed'**
  String get ornSpeed;

  /// No description provided for @accent.
  ///
  /// In en, this message translates to:
  /// **'Accent'**
  String get accent;

  /// No description provided for @accentHint.
  ///
  /// In en, this message translates to:
  /// **'Punches the start of the note, up to +6 dB, then settles back.'**
  String get accentHint;

  /// No description provided for @accentAmount.
  ///
  /// In en, this message translates to:
  /// **'Strength'**
  String get accentAmount;

  /// No description provided for @clearExpression.
  ///
  /// In en, this message translates to:
  /// **'Clear'**
  String get clearExpression;

  /// No description provided for @histOrnament.
  ///
  /// In en, this message translates to:
  /// **'Ornament'**
  String get histOrnament;

  /// No description provided for @histAccent.
  ///
  /// In en, this message translates to:
  /// **'Accent'**
  String get histAccent;

  /// No description provided for @histExprClear.
  ///
  /// In en, this message translates to:
  /// **'Clear ornament'**
  String get histExprClear;

  /// No description provided for @metaAccent.
  ///
  /// In en, this message translates to:
  /// **'accent {percent}%'**
  String metaAccent(int percent);

  /// No description provided for @advAccentBeats.
  ///
  /// In en, this message translates to:
  /// **'Accent on the beat'**
  String get advAccentBeats;

  /// No description provided for @advAccentBeatsHint.
  ///
  /// In en, this message translates to:
  /// **'Accents notes that start on a beat, for a tighter groove'**
  String get advAccentBeatsHint;
}

class _AppLocalizationsDelegate
    extends LocalizationsDelegate<AppLocalizations> {
  const _AppLocalizationsDelegate();

  @override
  Future<AppLocalizations> load(Locale locale) {
    return SynchronousFuture<AppLocalizations>(lookupAppLocalizations(locale));
  }

  @override
  bool isSupported(Locale locale) =>
      <String>['en', 'th'].contains(locale.languageCode);

  @override
  bool shouldReload(_AppLocalizationsDelegate old) => false;
}

AppLocalizations lookupAppLocalizations(Locale locale) {
  // Lookup logic when only language code is specified.
  switch (locale.languageCode) {
    case 'en':
      return AppLocalizationsEn();
    case 'th':
      return AppLocalizationsTh();
  }

  throw FlutterError(
    'AppLocalizations.delegate failed to load unsupported locale "$locale". This is likely '
    'an issue with the localizations generation tool. Please file an issue '
    'on GitHub with a reproducible sample app and the gen-l10n configuration '
    'that was used.',
  );
}
