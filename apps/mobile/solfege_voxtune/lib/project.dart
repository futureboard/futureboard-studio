import 'dart:convert';
import 'dart:io';

import 'package:flutter/services.dart';

/// Saved edit state of one note, keyed by its analysis id.
class NoteEdit {
  final double pitch;
  final double start;
  final double dur;
  final double modulation;
  final double formant;
  final double gainDb;
  final bool pinned;

  const NoteEdit({
    required this.pitch,
    required this.start,
    required this.dur,
    required this.modulation,
    required this.formant,
    required this.gainDb,
    required this.pinned,
  });

  factory NoteEdit.fromJson(Map<String, dynamic> j) => NoteEdit(
    pitch: (j['pitch'] as num).toDouble(),
    start: (j['start'] as num).toDouble(),
    dur: (j['dur'] as num).toDouble(),
    modulation: (j['modulation'] as num? ?? 1).toDouble(),
    formant: (j['formant'] as num? ?? 0).toDouble(),
    gainDb: (j['gainDb'] as num? ?? 0).toDouble(),
    pinned: j['pinned'] as bool? ?? false,
  );

  Map<String, dynamic> toJson() => {
    'pitch': pitch,
    'start': start,
    'dur': dur,
    'modulation': modulation,
    'formant': formant,
    'gainDb': gainDb,
    'pinned': pinned,
  };
}

/// One audio file in a project, with everything needed to restore its edits.
/// The audio is copied into the project's `Audio/` folder on import so the
/// project stays self-contained.
class ProjectFile {
  final String id;
  String name;

  /// Path relative to the project folder, e.g. `Audio/lead.wav`.
  final String audio;

  /// Null until the file has been analysed and edited once.
  String? key;
  String? mode;
  double correct;

  /// Legacy (format 1 before cut/join): per-note edits keyed by analysis id.
  Map<String, NoteEdit> edits;

  /// The full edited note list (JSON, as the engine uses it). Needed since
  /// cut/joined notes can't be recreated by re-analysis. Null if never saved.
  List<Map<String, dynamic>>? notes;

  ProjectFile({
    required this.id,
    required this.name,
    required this.audio,
    this.key,
    this.mode,
    this.correct = 0,
    Map<String, NoteEdit>? edits,
    this.notes,
  }) : edits = edits ?? {};

  factory ProjectFile.fromJson(Map<String, dynamic> j) => ProjectFile(
    id: j['id'] as String,
    name: j['name'] as String,
    audio: j['audio'] as String,
    key: j['key'] as String?,
    mode: j['mode'] as String?,
    correct: (j['correct'] as num? ?? 0).toDouble(),
    edits: ((j['edits'] as Map<String, dynamic>?) ?? const {}).map(
      (k, v) => MapEntry(k, NoteEdit.fromJson(v as Map<String, dynamic>)),
    ),
    notes: (j['notes'] as List<dynamic>?)
        ?.map((e) => (e as Map).cast<String, dynamic>())
        .toList(),
  );

  Map<String, dynamic> toJson() => {
    'id': id,
    'name': name,
    'audio': audio,
    'key': key,
    'mode': mode,
    'correct': correct,
    if (edits.isNotEmpty) 'edits': edits.map((k, v) => MapEntry(k, v.toJson())),
    if (notes != null) 'notes': notes,
  };
}

class Project {
  static const int formatVersion = 1;
  static const String fileName = 'project.json';

  /// Absolute path of the project folder.
  final String dir;
  String name;
  final DateTime created;
  DateTime modified;
  final List<ProjectFile> files;

  Project({
    required this.dir,
    required this.name,
    required this.created,
    required this.modified,
    List<ProjectFile>? files,
  }) : files = files ?? [];

  String get audioDir => '$dir${Platform.pathSeparator}Audio';
  String get exportDir => '$dir${Platform.pathSeparator}Exports';
  String pathOf(ProjectFile f) =>
      '$dir${Platform.pathSeparator}${f.audio.replaceAll('/', Platform.pathSeparator)}';

  factory Project.fromJson(String dir, Map<String, dynamic> j) => Project(
    dir: dir,
    name: j['name'] as String,
    created: DateTime.parse(j['created'] as String),
    modified: DateTime.parse(j['modified'] as String),
    files: ((j['files'] as List<dynamic>?) ?? const [])
        .map((e) => ProjectFile.fromJson(e as Map<String, dynamic>))
        .toList(),
  );

  Map<String, dynamic> toJson() => {
    'app': 'Solfege Voxtune',
    'version': formatVersion,
    'name': name,
    'created': created.toIso8601String(),
    'modified': modified.toIso8601String(),
    'files': files.map((f) => f.toJson()).toList(),
  };

  /// Unique id for a new file within this project.
  String nextFileId() {
    var n = files.length + 1;
    while (files.any((f) => f.id == 'f$n')) {
      n++;
    }
    return 'f$n';
  }
}

/// Projects live in `Documents/Solfege Voxtune/<project>/`.
///
/// I/O is synchronous on purpose: project files are small, and keeping
/// saves synchronous makes them deterministic (no lost writes on exit).
class ProjectStore {
  static const String folderName = 'Solfege Voxtune';
  static const MethodChannel _channel = MethodChannel('canto/storage');

  final String root;

  ProjectStore(this.root);

  /// Resolves the projects root: on Android the shared
  /// `Documents/Solfege Voxtune` (asks for storage permission on Android
  /// 10 and older); elsewhere `~/Documents/Solfege Voxtune`.
  static Future<ProjectStore> open() async {
    String? path;
    if (Platform.isAndroid) {
      try {
        path = await _channel.invokeMethod<String>('documentsDir');
      } catch (_) {}
    }
    if (path == null || path.isEmpty) {
      final home =
          Platform.environment['USERPROFILE'] ??
          Platform.environment['HOME'] ??
          Directory.systemTemp.path;
      path =
          '$home${Platform.pathSeparator}Documents'
          '${Platform.pathSeparator}$folderName';
    }
    Directory(path).createSync(recursive: true);
    return ProjectStore(path);
  }

  /// Projects sorted by last modification, newest first. Unreadable folders
  /// are skipped rather than failing the whole list.
  List<Project> list() {
    final dir = Directory(root);
    if (!dir.existsSync()) return [];
    final projects = <Project>[];
    for (final entry in dir.listSync().whereType<Directory>()) {
      final file = File(
        '${entry.path}${Platform.pathSeparator}${Project.fileName}',
      );
      if (!file.existsSync()) continue;
      try {
        final json =
            jsonDecode(file.readAsStringSync()) as Map<String, dynamic>;
        projects.add(Project.fromJson(entry.path, json));
      } catch (_) {}
    }
    projects.sort((a, b) => b.modified.compareTo(a.modified));
    return projects;
  }

  Project create(String name) {
    final clean = _safeName(name.trim().isEmpty ? 'Project' : name.trim());
    var folder = '$root${Platform.pathSeparator}$clean';
    var n = 2;
    while (Directory(folder).existsSync()) {
      folder = '$root${Platform.pathSeparator}$clean ($n)';
      n++;
    }
    final now = DateTime.now();
    final project = Project(
      dir: folder,
      name: name.trim().isEmpty ? clean : name.trim(),
      created: now,
      modified: now,
    );
    Directory(project.audioDir).createSync(recursive: true);
    save(project);
    return project;
  }

  /// Atomic save: write a temp file, then rename over `project.json`.
  void save(Project project) {
    project.modified = DateTime.now();
    final target = '${project.dir}${Platform.pathSeparator}${Project.fileName}';
    final tmp = File('$target.tmp');
    tmp.writeAsStringSync(
      const JsonEncoder.withIndent('  ').convert(project.toJson()),
      flush: true,
    );
    tmp.renameSync(target);
  }

  void delete(Project project) {
    final dir = Directory(project.dir);
    // Only ever delete folders inside the store root.
    if (dir.existsSync() && dir.parent.path == Directory(root).path) {
      dir.deleteSync(recursive: true);
    }
  }

  /// Copies an audio file into the project and registers it.
  ProjectFile importFile(Project project, String sourcePath, String name) {
    Directory(project.audioDir).createSync(recursive: true);
    final target = _uniquePath(project.audioDir, _safeName(name));
    File(sourcePath).copySync(target);
    return _register(project, target, name);
  }

  /// Writes in-memory audio bytes into the project and registers them.
  ProjectFile importBytes(Project project, List<int> bytes, String name) {
    Directory(project.audioDir).createSync(recursive: true);
    final target = _uniquePath(project.audioDir, _safeName(name));
    File(target).writeAsBytesSync(bytes, flush: true);
    return _register(project, target, name);
  }

  /// Path for a new recording, e.g. `<project>/Audio/Take 3.wav`, numbered
  /// after the takes already in the project. [label] builds the visible name
  /// (localised "Take n").
  ({String path, String name}) newRecording(
    Project project,
    String Function(int n) label,
  ) {
    Directory(project.audioDir).createSync(recursive: true);
    var n = project.files.length + 1;
    while (true) {
      final name = '${label(n)}.wav';
      final path =
          '${project.audioDir}${Platform.pathSeparator}${_safeName(name)}';
      if (!File(path).existsSync() &&
          !project.files.any((f) => f.name == name)) {
        return (path: path, name: name);
      }
      n++;
    }
  }

  /// Registers a file already written inside the project (a recording).
  ProjectFile registerFile(Project project, String absolutePath, String name) =>
      _register(project, absolutePath, name);

  void removeFile(Project project, ProjectFile file) {
    project.files.remove(file);
    final audio = File(project.pathOf(file));
    if (audio.existsSync()) audio.deleteSync();
    save(project);
  }

  /// Unused export path for [file]: `Exports/<name> (tuned).wav`.
  String exportPathFor(Project project, ProjectFile file) {
    Directory(project.exportDir).createSync(recursive: true);
    final dot = file.name.lastIndexOf('.');
    final stem = dot > 0 ? file.name.substring(0, dot) : file.name;
    return '${project.exportDir}${Platform.pathSeparator}'
        '${_safeName(stem)} (tuned).wav';
  }

  ProjectFile _register(Project project, String absolutePath, String name) {
    final relative = absolutePath
        .substring(project.dir.length + 1)
        .replaceAll(Platform.pathSeparator, '/');
    final file = ProjectFile(
      id: project.nextFileId(),
      name: name,
      audio: relative,
    );
    project.files.add(file);
    save(project);
    return file;
  }

  static String _uniquePath(String dir, String name) {
    var path = '$dir${Platform.pathSeparator}$name';
    final dot = name.lastIndexOf('.');
    final stem = dot > 0 ? name.substring(0, dot) : name;
    final ext = dot > 0 ? name.substring(dot) : '';
    var n = 2;
    while (File(path).existsSync()) {
      path = '$dir${Platform.pathSeparator}$stem ($n)$ext';
      n++;
    }
    return path;
  }

  static String _safeName(String name) =>
      name.replaceAll(RegExp(r'[\\/:*?"<>|\x00-\x1F]'), '_').trim();
}
