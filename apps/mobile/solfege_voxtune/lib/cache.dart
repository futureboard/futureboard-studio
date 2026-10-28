import 'dart:io';
import 'dart:convert';

import 'project.dart';

/// Bump when the Rust analysis output for the same audio changes
/// (`CantoEngine::analyze_loaded_audio`), so old entries are re-analysed.
const int kAnalysisCacheVersion = 1;

/// Per-project cache in `<project>/Cache/`. Everything in it can be lost
/// without losing work (the project file holds the edits); it exists so
/// files open without re-analysis and undo history survives the app closing.
///
/// - `<fileId>.analysis`: a fingerprint line of the source audio, then the
///   engine's analysis JSON.
/// - `<fileId>.history.json`: the undo/redo history (`history.dart`).
///
/// Every operation is best effort: a failing cache never blocks editing.
class ProjectCache {
  static const String folderName = 'Cache';

  final Project project;

  ProjectCache(this.project);

  String get dir => '${project.dir}${Platform.pathSeparator}$folderName';

  File _file(ProjectFile f, String ext) =>
      File('$dir${Platform.pathSeparator}${f.id}.$ext');

  /// Identifies the exact audio an entry was made from. File ids can be
  /// reused after a removal, and audio can be replaced outside the app.
  String? _fingerprint(ProjectFile f) {
    final stat = File(project.pathOf(f)).statSync();
    if (stat.type == FileSystemEntityType.notFound) return null;
    return jsonEncode({
      'v': kAnalysisCacheVersion,
      'audio': f.audio,
      'size': stat.size,
      'modified': stat.modified.millisecondsSinceEpoch,
    });
  }

  /// The cached analysis JSON for [f], or null if missing or stale.
  String? readAnalysis(ProjectFile f) {
    try {
      final file = _file(f, 'analysis');
      if (!file.existsSync()) return null;
      final text = file.readAsStringSync();
      final newline = text.indexOf('\n');
      if (newline < 0) return null;
      final fingerprint = _fingerprint(f);
      if (fingerprint == null || text.substring(0, newline) != fingerprint) {
        return null;
      }
      return text.substring(newline + 1);
    } catch (_) {
      return null;
    }
  }

  void writeAnalysis(ProjectFile f, String analysisJson) {
    try {
      final fingerprint = _fingerprint(f);
      if (fingerprint == null) return;
      _writeAtomic(_file(f, 'analysis'), '$fingerprint\n$analysisJson');
    } catch (_) {}
  }

  /// The saved history document for [f], or null.
  Map<String, dynamic>? readHistory(ProjectFile f) {
    try {
      final file = _file(f, 'history.json');
      if (!file.existsSync()) return null;
      return jsonDecode(file.readAsStringSync()) as Map<String, dynamic>;
    } catch (_) {
      return null;
    }
  }

  void writeHistory(ProjectFile f, String json) {
    try {
      _writeAtomic(_file(f, 'history.json'), json);
    } catch (_) {}
  }

  void deleteHistory(ProjectFile f) => _delete(_file(f, 'history.json'));

  /// Drops everything cached for [f] (the file was removed).
  void remove(ProjectFile f) {
    _delete(_file(f, 'analysis'));
    _delete(_file(f, 'history.json'));
  }

  void _writeAtomic(File target, String contents) {
    Directory(dir).createSync(recursive: true);
    final tmp = File('${target.path}.tmp');
    tmp.writeAsStringSync(contents, flush: true);
    tmp.renameSync(target.path);
  }

  static void _delete(File f) {
    try {
      if (f.existsSync()) f.deleteSync();
    } catch (_) {}
  }

  /// Cache folders of every project in [store].
  static Iterable<Directory> _allDirs(ProjectStore store) sync* {
    final root = Directory(store.root);
    if (!root.existsSync()) return;
    for (final project in root.listSync().whereType<Directory>()) {
      final cache = Directory(
        '${project.path}${Platform.pathSeparator}$folderName',
      );
      if (cache.existsSync()) yield cache;
    }
  }

  /// Total bytes cached across all projects.
  static int totalBytes(ProjectStore store) {
    var total = 0;
    try {
      for (final dir in _allDirs(store)) {
        for (final f in dir.listSync().whereType<File>()) {
          total += f.lengthSync();
        }
      }
    } catch (_) {}
    return total;
  }

  /// Deletes the cache of every project. Files are analysed again when next
  /// opened; undo history of projects not open right now is gone.
  static void clearAll(ProjectStore store) {
    for (final dir in _allDirs(store).toList()) {
      try {
        dir.deleteSync(recursive: true);
      } catch (_) {}
    }
  }
}

/// "1.2 MB"-style size for the Settings page.
String formatBytes(int bytes) {
  if (bytes < 1024) return '$bytes B';
  if (bytes < 1024 * 1024) return '${(bytes / 1024).toStringAsFixed(0)} KB';
  return '${(bytes / (1024 * 1024)).toStringAsFixed(1)} MB';
}
