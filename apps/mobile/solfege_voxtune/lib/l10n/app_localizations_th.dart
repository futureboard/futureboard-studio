// ignore: unused_import
import 'package:intl/intl.dart' as intl;

import 'app_localizations.dart';

// ignore_for_file: type=lint

/// The translations for Thai (`th`).
class AppLocalizationsTh extends AppLocalizations {
  AppLocalizationsTh([String locale = 'th']) : super(locale);

  @override
  String get noteCut => 'ตัด';

  @override
  String get noteJoin => 'รวม';

  @override
  String get noteMute => 'ปิดเสียง';

  @override
  String get noteUnmute => 'เปิดเสียง';

  @override
  String get noteReset => 'รีเซ็ต';

  @override
  String get cutHint => 'ลากผ่านโน้ต แล้วยกนิ้วเพื่อตัด';

  @override
  String get done => 'เสร็จ';

  @override
  String get noteJoinSemantic => 'รวมกับโน้ตถัดไป';

  @override
  String get muted => 'ปิดเสียงอยู่';

  @override
  String get advancedTools => 'เครื่องมือขั้นสูง';

  @override
  String get advScope => 'ใช้กับ';

  @override
  String get advScopeAll => 'ทุกโน้ต';

  @override
  String get advScopeSelected => 'โน้ตที่เลือก';

  @override
  String get advCenter => 'แก้พิตช์กลางโน้ต';

  @override
  String get advCenterHint => 'ดึงแต่ละโน้ตเข้าหาโน้ตในสเกลที่ใกล้ที่สุด';

  @override
  String get advDrift => 'ลดการส่ายของพิตช์';

  @override
  String get advDriftHint => 'ทำให้เสียงที่ส่ายและไวบราโตในโน้ตนิ่งขึ้น';

  @override
  String get advQuantize => 'จัดจังหวะเข้ากริด';

  @override
  String get advQuantizeHint => 'เลื่อนจุดเริ่มของโน้ตเข้าหาจังหวะ';

  @override
  String get advGrid => 'กริด';

  @override
  String get advStrength => 'ความแรง';

  @override
  String get advTranspose => 'ทรานสโพส';

  @override
  String get advTransposeHint =>
      'เลื่อนพิตช์เป็นครึ่งเสียง ฟอร์แมนต์ยังเป็นธรรมชาติ';

  @override
  String get advLevel => 'เกลี่ยความดัง';

  @override
  String get advLevelHint => 'ทำให้โน้ตที่ดังกับเบาใกล้กันมากขึ้น';

  @override
  String get advAmount => 'ปริมาณ';

  @override
  String get apply => 'ใช้';

  @override
  String advApplied(int count) {
    String _temp0 = intl.Intl.pluralLogic(
      count,
      locale: localeName,
      other: 'ใช้กับ $count โน้ตแล้ว',
      zero: 'ไม่มีโน้ตที่เปลี่ยน',
    );
    return '$_temp0';
  }

  @override
  String get record => 'อัดเสียง';

  @override
  String get recordTake => 'อัดเสียงเทคใหม่';

  @override
  String get recordTakeHint =>
      'ร้องใส่ไมโครโฟน — พอหยุด เทคจะเปิดขึ้นมาที่นี่ทันที';

  @override
  String get recordTapToStart => 'แตะเพื่อเริ่มอัด';

  @override
  String get recordTapToStop => 'แตะเพื่อหยุด — เทคจะเปิดในหน้าแก้ไข';

  @override
  String get recordSingNote => 'ลองร้องสักโน้ต';

  @override
  String get recordTooLoud => 'เสียงดังเกินไป — ถอยห่างจากไมค์อีกนิด';

  @override
  String get recordNeedsMic => 'ต้องอนุญาตให้ใช้ไมโครโฟนก่อนจึงจะอัดเสียงได้';

  @override
  String get recordAllowMic => 'อนุญาตไมโครโฟน';

  @override
  String recordFailed(String error) {
    return 'อัดเสียงไม่ได้: $error';
  }

  @override
  String get recordSaving => 'กำลังบันทึก…';

  @override
  String get discardTakeTitle => 'ทิ้งเทคนี้ใช่ไหม';

  @override
  String get discardTakeBody => 'เสียงที่อัดไว้จนถึงตอนนี้จะถูกลบ';

  @override
  String get discard => 'ทิ้ง';

  @override
  String get keepRecording => 'อัดต่อ';

  @override
  String takeName(int n) {
    return 'เทค $n';
  }

  @override
  String get appName => 'Solfege Voxtune';

  @override
  String get projects => 'โปรเจกต์';

  @override
  String get newProject => 'โปรเจกต์ใหม่';

  @override
  String get projectName => 'ชื่อโปรเจกต์';

  @override
  String get untitledProject => 'โปรเจกต์ไม่มีชื่อ';

  @override
  String get create => 'สร้าง';

  @override
  String get cancel => 'ยกเลิก';

  @override
  String get delete => 'ลบ';

  @override
  String get remove => 'นำออก';

  @override
  String get homeEmptyTitle => 'ยังไม่มีโปรเจกต์';

  @override
  String get homeEmptyBody =>
      'สร้างโปรเจกต์ เพิ่มไฟล์เสียงร้อง แล้วทุกการแก้ไขจะถูกบันทึกให้อัตโนมัติ';

  @override
  String projectFiles(int count) {
    String _temp0 = intl.Intl.pluralLogic(
      count,
      locale: localeName,
      other: '$count ไฟล์',
      zero: 'ไม่มีไฟล์',
    );
    return '$_temp0';
  }

  @override
  String get deleteProject => 'ลบโปรเจกต์';

  @override
  String deleteProjectTitle(String name) {
    return 'ลบ “$name” ใช่ไหม';
  }

  @override
  String get deleteProjectBody =>
      'โฟลเดอร์โปรเจกต์ รวมถึงสำเนาไฟล์เสียงและไฟล์ที่ส่งออก จะถูกลบออกจากเครื่องนี้';

  @override
  String savedIn(String path) {
    return 'บันทึกไว้ที่ $path';
  }

  @override
  String storageError(String error) {
    return 'เปิดโฟลเดอร์โปรเจกต์ไม่ได้: $error';
  }

  @override
  String get openMenu => 'เปิดเมนู';

  @override
  String get files => 'ไฟล์';

  @override
  String get addAudioFile => 'เพิ่มไฟล์เสียง';

  @override
  String get notOpenedYet => 'ยังไม่ได้เปิด';

  @override
  String get allProjects => 'โปรเจกต์ทั้งหมด';

  @override
  String get settings => 'ตั้งค่า';

  @override
  String get export => 'ส่งออก';

  @override
  String get emptyProjectTitle => 'เพิ่มไฟล์เสียงร้อง';

  @override
  String get emptyProjectBody =>
      'โปรเจกต์นี้ยังไม่มีเสียง เพิ่มไฟล์เสียงร้องแบบโมโนที่ไม่มีเอฟเฟกต์ — Solfege Voxtune จะตรวจหาทุกโน้ต คีย์ และเทมโปให้ แล้วคุณแก้พิตช์และจังหวะเองได้';

  @override
  String get removeFile => 'นำออกจากโปรเจกต์';

  @override
  String get removeFileHint => 'ลบสำเนาไฟล์นี้และการแก้ไขทั้งหมดของมัน';

  @override
  String removeFileTitle(String name) {
    return 'นำ “$name” ออกใช่ไหม';
  }

  @override
  String get removeFileBody =>
      'สำเนาไฟล์เสียงในโปรเจกต์นี้และการแก้ไขทั้งหมดจะถูกลบ ไฟล์ต้นฉบับในเครื่องจะไม่ถูกแตะต้อง';

  @override
  String get exportThis => 'ส่งออกไฟล์นี้';

  @override
  String get exportThisHint => 'WAV · 48 kHz · โมโน พร้อมการแก้ไขทั้งหมด';

  @override
  String get exportAll => 'ส่งออกทุกไฟล์';

  @override
  String exportAllHint(int count) {
    String _temp0 = intl.Intl.pluralLogic(
      count,
      locale: localeName,
      other: '$count ไฟล์',
    );
    return '$_temp0 ไปที่โฟลเดอร์ Exports ของโปรเจกต์';
  }

  @override
  String get exporting => 'กำลังส่งออก…';

  @override
  String exportDone(String path) {
    return 'บันทึกไว้ที่ $path';
  }

  @override
  String exportAllDone(int count, String path) {
    String _temp0 = intl.Intl.pluralLogic(
      count,
      locale: localeName,
      other: 'ส่งออก $count ไฟล์แล้ว',
    );
    return '$_temp0 ไปที่ $path';
  }

  @override
  String exportFailed(String error) {
    return 'ส่งออกไม่สำเร็จ: $error';
  }

  @override
  String get defaultSnap => 'สแนปพิตช์เริ่มต้น';

  @override
  String get defaultSnapHint => 'ใช้เมื่อเปิดโปรเจกต์';

  @override
  String get storage => 'พื้นที่จัดเก็บ';

  @override
  String get storageHint =>
      'โปรเจกต์ สำเนาไฟล์เสียง และไฟล์ที่ส่งออกจะถูกบันทึกที่นี่';

  @override
  String get about => 'เกี่ยวกับ';

  @override
  String version(String version) {
    return 'เวอร์ชัน $version';
  }

  @override
  String get aboutBody =>
      'โปรแกรมแก้พิตช์และจังหวะเสียงร้อง เอนจินเสียงเขียนด้วย Rust: วิเคราะห์พิตช์ด้วย YIN และปรับพิตช์/ฟอร์แมนต์ด้วย TD-PSOLA ประมวลผลบนเธรดเสียงแบบเนทีฟ';

  @override
  String get noAudioLoaded => 'ยังไม่ได้โหลดเสียง';

  @override
  String get audioClip => 'คลิปเสียง';

  @override
  String get loadFailed => 'ถอดรหัสหรือวิเคราะห์ไฟล์เสียงไม่สำเร็จ';

  @override
  String get toolPitch => 'พิตช์';

  @override
  String get toolDrift => 'ดริฟต์';

  @override
  String get toolTime => 'จังหวะ';

  @override
  String get toolFormant => 'ฟอร์แมนต์';

  @override
  String get toolLevel => 'ความดัง';

  @override
  String toolSemantic(String tool) {
    return 'เครื่องมือ$tool';
  }

  @override
  String get modeMinor => 'ไมเนอร์';

  @override
  String get modeMajor => 'เมเจอร์';

  @override
  String get modePenta => 'เพนทาโทนิก';

  @override
  String get modeTitleMinor => 'ไมเนอร์';

  @override
  String get modeTitleMajor => 'เมเจอร์';

  @override
  String get modeTitlePenta => 'เพนทาโทนิก';

  @override
  String keyChip(String key, String mode) {
    return '$key $mode';
  }

  @override
  String get snapFree => 'อิสระ';

  @override
  String get snapSemi => 'ครึ่งเสียง';

  @override
  String get snapScale => 'สเกล';

  @override
  String snapSemantic(String mode) {
    return 'สแนปพิตช์: $mode';
  }

  @override
  String get inKey => 'ตรงคีย์';

  @override
  String offCount(int count) {
    return 'เพี้ยน $count';
  }

  @override
  String get nextOffSemantic => 'เลือกโน้ตเพี้ยนตัวถัดไป';

  @override
  String get keySheetTitle => 'คีย์และสเกล';

  @override
  String detectedKey(String key, String mode) {
    return 'ตรวจพบ $key $mode';
  }

  @override
  String get rootLabel => 'โน้ตหลัก';

  @override
  String get scaleLabel => 'สเกล';

  @override
  String get keySheetNote =>
      'การแก้เพี้ยนและการสแนปจะยึดตามสเกลนี้ ส่วนโน้ตที่คุณแก้เองจะคงพิตช์เดิมไว้';

  @override
  String get sessionLength => 'ความยาว';

  @override
  String get sessionTempo => 'เทมโป';

  @override
  String get sessionKey => 'คีย์';

  @override
  String get sessionNotes => 'โน้ต';

  @override
  String bpmValue(int bpm) {
    return '$bpm BPM';
  }

  @override
  String get openAudioFile => 'เปิดไฟล์เสียง';

  @override
  String get openAudioFileHint =>
      'WAV, MP3, FLAC, OGG หรือ M4A — เสียงร้องแบบโมโนได้ผลดีที่สุด';

  @override
  String get reanalyze => 'วิเคราะห์ใหม่';

  @override
  String get reanalyzeHint =>
      'ตรวจหาโน้ต คีย์ และเทมโปอีกครั้ง การแก้ไขทั้งหมดจะถูกล้าง';

  @override
  String get resetAll => 'รีเซ็ตการแก้ไขทั้งหมด';

  @override
  String get resetAllHint => 'คืนทุกโน้ตกลับเป็นพิตช์และจังหวะที่ตรวจพบ';

  @override
  String get language => 'ภาษา';

  @override
  String get languageSystem => 'ตามระบบ';

  @override
  String get undo => 'เลิกทำ';

  @override
  String get redo => 'ทำซ้ำ';

  @override
  String get zoomIn => 'ซูมเข้า';

  @override
  String get zoomOut => 'ซูมออก';

  @override
  String get emptyErrorTitle => 'เปิดไฟล์นี้ไม่ได้';

  @override
  String get emptyNoNotesTitle => 'ไม่พบโน้ตที่ร้อง';

  @override
  String get emptyTitle => 'จูนเสียงร้องของคุณ';

  @override
  String get emptyNoNotesBody =>
      'คลิปนี้ไม่มีท่อนที่มีพิตช์ดังกว่าเสียงรบกวน ลองใช้เสียงร้องแบบโมโนที่ไม่มีเอฟเฟกต์';

  @override
  String get emptyBody =>
      'เปิดไฟล์เสียงร้องแบบโมโนที่ไม่มีเอฟเฟกต์ Solfege Voxtune จะตรวจหาทุกโน้ต คีย์ และเทมโปให้ แล้วคุณแก้พิตช์และจังหวะเองได้';

  @override
  String get analyzing => 'กำลังวิเคราะห์…';

  @override
  String get correctLabel => 'แก้เพี้ยน';

  @override
  String get tuneAll => 'จูนทั้งหมด';

  @override
  String get snap => 'สแนป';

  @override
  String get revert => 'ย้อนคืน';

  @override
  String get inspBreath => 'เสียงหายใจ / เสียงรบกวน';

  @override
  String get inspNoNotes => 'ไม่พบโน้ต';

  @override
  String get inspAllInKey => 'ทุกโน้ตตรงคีย์';

  @override
  String inspOffScale(int count) {
    String _temp0 = intl.Intl.pluralLogic(
      count,
      locale: localeName,
      other: 'มี $count โน้ตหลุดสเกล',
    );
    return '$_temp0';
  }

  @override
  String get inspUnpitchedHint => 'ไม่มีพิตช์ — ใช้เครื่องมือพิตช์ไม่ได้';

  @override
  String get inspOpenHint => 'เปิดไฟล์เสียงร้องเพื่อเริ่มต้น';

  @override
  String get inspTapHint => 'แตะโน้ตเพื่อดูรายละเอียดและแก้ไข';

  @override
  String metaBar(String position) {
    return 'ห้อง $position';
  }

  @override
  String metaWas(String cents) {
    return 'เดิม $cents';
  }

  @override
  String get metaEdited => 'แก้แล้ว';

  @override
  String get backToStart => 'กลับไปจุดเริ่มต้น';

  @override
  String get play => 'เล่น';

  @override
  String get pause => 'หยุดชั่วคราว';

  @override
  String get loop => 'วนซ้ำ';

  @override
  String get bypassSemantic => 'ฟังเสียงต้นฉบับ';

  @override
  String get bypassOn => 'ต้นฉบับ';

  @override
  String get bypassOff => 'บายพาส';

  @override
  String get bypassTag => 'ต้นฉบับ';

  @override
  String get history => 'ประวัติการแก้ไข';

  @override
  String get historyHint =>
      'แตะขั้นตอนเพื่อย้อนกลับไปจุดนั้น ประวัติยังอยู่แม้ปิดแอป';

  @override
  String get historyEmpty => 'ยังไม่มีการแก้ไข';

  @override
  String get historyStart => 'จุดเริ่มต้น';

  @override
  String get historyNow => 'ตอนนี้';

  @override
  String historyStepsHint(int count) {
    String _temp0 = intl.Intl.pluralLogic(
      count,
      locale: localeName,
      other: '$count ขั้นตอน · ย้อนไปจุดไหนก็ได้',
      zero: 'ยังไม่มีการแก้ไข',
    );
    return '$_temp0';
  }

  @override
  String undoToast(String action) {
    return 'เลิกทำ · $action';
  }

  @override
  String redoToast(String action) {
    return 'ทำซ้ำ · $action';
  }

  @override
  String histKey(String name) {
    return 'คีย์ $name';
  }

  @override
  String histScale(String name) {
    return 'สเกล $name';
  }

  @override
  String get histSnapNote => 'จับโน้ตเข้าสเกล';

  @override
  String get histCorrect => 'ปรับระดับการแก้';

  @override
  String get histCut => 'ตัดโน้ต';

  @override
  String get histJoin => 'รวมโน้ต';

  @override
  String get histMute => 'ปิดเสียงโน้ต';

  @override
  String get histUnmute => 'เปิดเสียงโน้ต';

  @override
  String get histReset => 'รีเซ็ตโน้ต';

  @override
  String get histEdit => 'แก้ไข';

  @override
  String get cache => 'แคช';

  @override
  String get cacheHint =>
      'ผลวิเคราะห์เสียงและประวัติการแก้ไขที่เก็บไว้ ช่วยให้เปิดไฟล์ได้ทันที ล้างแล้วงานที่แก้ไว้ยังอยู่ครบ แต่ไฟล์จะถูกวิเคราะห์ใหม่และประวัติการแก้ไขจะเริ่มใหม่';

  @override
  String cacheUsed(String size) {
    return 'ใช้พื้นที่ $size';
  }

  @override
  String get clearCache => 'ล้างแคช';

  @override
  String get cacheCleared => 'ล้างแคชแล้ว';

  @override
  String get loadingProject => 'กำลังเปิดโปรเจกต์';

  @override
  String get loadingFile => 'กำลังเปิดไฟล์';

  @override
  String get reanalyzingFile => 'กำลังวิเคราะห์ใหม่';

  @override
  String get loadStageDecode => 'กำลังอ่านไฟล์เสียง…';

  @override
  String get loadStageAnalyze => 'กำลังวิเคราะห์พิตช์และโน้ต';

  @override
  String get loadStageRestore => 'กำลังกู้คืนงานที่แก้ไว้…';

  @override
  String get loadHintCached => 'ใช้ผลวิเคราะห์ที่เก็บไว้ จึงเปิดได้เร็ว';

  @override
  String get loadHintFirst =>
      'เปิดไฟล์นี้ครั้งแรกจึงต้องวิเคราะห์ ครั้งหน้าจะเปิดได้ทันที';

  @override
  String get cancelling => 'กำลังยกเลิก…';

  @override
  String get notOpenedBody =>
      'ยกเลิกการเปิดไว้ เปิดอีกครั้งเพื่อวิเคราะห์และแก้ไข';

  @override
  String get openFile => 'เปิดไฟล์';

  @override
  String get noteExpression => 'ลูกคอ';

  @override
  String get expressionTitle => 'ลูกคอและ Accent';

  @override
  String get listen => 'ฟัง';

  @override
  String get ornament => 'ลูกคอ';

  @override
  String get ornNone => 'ไม่มี';

  @override
  String get ornTurnUp => 'สะบัดขึ้น';

  @override
  String get ornTurnDown => 'สะบัดลง';

  @override
  String get ornShake => 'ลูกคอสั่น';

  @override
  String get ornFall => 'ปัดท้าย';

  @override
  String get ornDepth => 'ความกว้าง';

  @override
  String get ornSpeed => 'ความเร็ว';

  @override
  String get accent => 'Accent (เน้นหัวโน้ต)';

  @override
  String get accentHint =>
      'เน้นช่วงต้นโน้ตให้ชัดขึ้น สูงสุด +6 dB แล้วค่อยกลับสู่ระดับเดิม';

  @override
  String get accentAmount => 'ความแรง';

  @override
  String get clearExpression => 'ล้าง';

  @override
  String get histOrnament => 'ลูกคอ';

  @override
  String get histAccent => 'เน้นหัวโน้ต';

  @override
  String get histExprClear => 'ล้างลูกคอ';

  @override
  String metaAccent(int percent) {
    return 'เน้น $percent%';
  }

  @override
  String get advAccentBeats => 'เน้นโน้ตที่ตกจังหวะ';

  @override
  String get advAccentBeatsHint =>
      'ใส่ accent ให้โน้ตที่เริ่มตรงจังหวะ ให้ groove แน่นขึ้น';
}
