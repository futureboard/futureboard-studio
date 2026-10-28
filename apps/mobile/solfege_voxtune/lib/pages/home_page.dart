import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../l10n/app_localizations.dart';
import '../main.dart' show CantoScreen;
import '../project.dart';
import '../theme.dart';
import '../ui/components.dart';
import 'settings_page.dart';

/// Start screen: the projects saved in `Documents/Solfege Voxtune`.
class HomePage extends StatefulWidget {
  final ProjectStore store;

  const HomePage({super.key, required this.store});

  @override
  State<HomePage> createState() => _HomePageState();
}

class _HomePageState extends State<HomePage> {
  List<Project> _projects = const [];

  @override
  void initState() {
    super.initState();
    _refresh();
  }

  void _refresh() => setState(() => _projects = widget.store.list());

  Future<void> _open(Project project) async {
    await Navigator.of(context).push(
      MaterialPageRoute<void>(
        builder: (_) => CantoScreen(project: project, store: widget.store),
      ),
    );
    if (mounted) _refresh(); // file counts and dates changed
  }

  Future<void> _create() async {
    final l = AppLocalizations.of(context);
    final name = await showDialog<String>(
      context: context,
      builder: (_) => _NameDialog(
        title: l.newProject,
        hint: l.projectName,
        initial: l.untitledProject,
        confirm: l.create,
      ),
    );
    if (name == null || !mounted) return;
    final project = widget.store.create(name);
    await _open(project);
  }

  Future<void> _delete(Project project) async {
    final l = AppLocalizations.of(context);
    final ok = await showDialog<bool>(
      context: context,
      builder: (dialogContext) => AlertDialog(
        backgroundColor: CantoColors.raised,
        title: Text(l.deleteProjectTitle(project.name)),
        content: Text(l.deleteProjectBody),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(dialogContext, false),
            child: Text(l.cancel),
          ),
          TextButton(
            key: const Key('confirm-delete'),
            onPressed: () => Navigator.pop(dialogContext, true),
            style: TextButton.styleFrom(foregroundColor: CantoColors.heat),
            child: Text(l.delete),
          ),
        ],
      ),
    );
    if (ok != true) return;
    widget.store.delete(project);
    _refresh();
  }

  @override
  Widget build(BuildContext context) {
    final l = AppLocalizations.of(context);
    return Scaffold(
      backgroundColor: CantoColors.surface,
      body: SafeArea(
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 12, 8, 4),
              child: Row(
                children: [
                  const CantoLogo(size: 36),
                  const SizedBox(width: 12),
                  Expanded(
                    child: Text(
                      l.appName,
                      style: CantoTypography.sans(
                        size: 20,
                        weight: FontWeight.w700,
                        color: CantoColors.inkBright,
                      ),
                    ),
                  ),
                  CantoIconButton(
                    key: const Key('home-settings'),
                    icon: LucideIcons.settings,
                    tooltip: l.settings,
                    onTap: () => Navigator.of(context).push(
                      MaterialPageRoute<void>(
                        builder: (_) => SettingsPage(store: widget.store),
                      ),
                    ),
                  ),
                ],
              ),
            ),
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 16, 16, 8),
              child: Row(
                children: [
                  Expanded(
                    child: Text(
                      l.projects.toUpperCase(),
                      style: CantoTypography.label(text: l.projects),
                    ),
                  ),
                  CantoButton(
                    key: const Key('new-project'),
                    style: CantoButtonStyle.primary,
                    icon: LucideIcons.plus,
                    label: l.newProject,
                    onTap: _create,
                  ),
                ],
              ),
            ),
            Expanded(
              child: _projects.isEmpty
                  ? _buildEmpty(l)
                  : LayoutBuilder(
                      builder: (context, constraints) {
                        // Tablets and landscape: cards in columns.
                        final inner = constraints.maxWidth - 32;
                        final columns = (inner / 400).floor().clamp(1, 3);
                        if (columns == 1) {
                          return ListView.separated(
                            padding: const EdgeInsets.fromLTRB(16, 4, 16, 16),
                            itemCount: _projects.length,
                            separatorBuilder: (_, _) =>
                                const SizedBox(height: 8),
                            itemBuilder: (_, i) =>
                                _buildProject(l, _projects[i]),
                          );
                        }
                        final itemWidth = (inner - 8 * (columns - 1)) / columns;
                        return SingleChildScrollView(
                          padding: const EdgeInsets.fromLTRB(16, 4, 16, 16),
                          child: Wrap(
                            spacing: 8,
                            runSpacing: 8,
                            children: [
                              for (final p in _projects)
                                SizedBox(
                                  width: itemWidth,
                                  child: _buildProject(l, p),
                                ),
                            ],
                          ),
                        );
                      },
                    ),
            ),
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 4, 16, 12),
              child: Text(
                l.savedIn(widget.store.root),
                maxLines: 2,
                overflow: TextOverflow.ellipsis,
                style: CantoTypography.sans(size: 11, color: CantoColors.faint),
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _buildEmpty(AppLocalizations l) {
    return Center(
      child: SingleChildScrollView(
        padding: const EdgeInsets.symmetric(horizontal: 32),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Container(
              width: 56,
              height: 56,
              decoration: BoxDecoration(
                color: CantoColors.raised,
                borderRadius: BorderRadius.circular(16),
                border: Border.all(color: CantoColors.line),
              ),
              child: const Icon(
                LucideIcons.folderOpen,
                size: 24,
                color: CantoColors.ink,
              ),
            ),
            const SizedBox(height: 14),
            Text(
              l.homeEmptyTitle,
              style: CantoTypography.sans(
                size: 17,
                weight: FontWeight.w600,
                color: CantoColors.inkBright,
              ),
            ),
            const SizedBox(height: 6),
            Text(
              l.homeEmptyBody,
              textAlign: TextAlign.center,
              style: CantoTypography.sans(
                size: 12.5,
                color: CantoColors.muted,
                height: 1.45,
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _buildProject(AppLocalizations l, Project project) {
    final date = MaterialLocalizations.of(context)
        .formatMediumDate(project.modified);
    return Pressable(
      key: Key('project-${project.name}'),
      onTap: () => _open(project),
      child: Container(
        padding: const EdgeInsets.fromLTRB(14, 12, 4, 12),
        decoration: BoxDecoration(
          color: CantoColors.raised,
          borderRadius: BorderRadius.circular(CantoRadii.card),
          border: Border.all(color: CantoColors.line),
        ),
        child: Row(
          children: [
            const Icon(
              LucideIcons.audioWaveform,
              size: 20,
              color: CantoColors.muted,
            ),
            const SizedBox(width: 12),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(
                    project.name,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: CantoTypography.sans(
                      size: 14.5,
                      weight: FontWeight.w600,
                      color: CantoColors.inkBright,
                    ),
                  ),
                  const SizedBox(height: 3),
                  Text(
                    '${l.projectFiles(project.files.length)} · $date',
                    style: CantoTypography.sans(
                      size: 11.5,
                      color: CantoColors.muted,
                    ),
                  ),
                ],
              ),
            ),
            CantoIconButton(
              key: Key('delete-${project.name}'),
              icon: LucideIcons.trash2,
              iconSize: 17,
              tooltip: l.deleteProject,
              color: CantoColors.muted,
              onTap: () => _delete(project),
            ),
          ],
        ),
      ),
    );
  }
}

class _NameDialog extends StatefulWidget {
  final String title;
  final String hint;
  final String initial;
  final String confirm;

  const _NameDialog({
    required this.title,
    required this.hint,
    required this.initial,
    required this.confirm,
  });

  @override
  State<_NameDialog> createState() => _NameDialogState();
}

class _NameDialogState extends State<_NameDialog> {
  late final TextEditingController _controller =
      TextEditingController(text: widget.initial)
        ..selection = TextSelection(
          baseOffset: 0,
          extentOffset: widget.initial.length,
        );

  @override
  void dispose() {
    _controller.dispose();
    super.dispose();
  }

  void _submit() => Navigator.pop(context, _controller.text.trim());

  @override
  Widget build(BuildContext context) {
    final l = AppLocalizations.of(context);
    return AlertDialog(
      backgroundColor: CantoColors.raised,
      title: Text(widget.title),
      content: TextField(
        key: const Key('project-name-field'),
        controller: _controller,
        autofocus: true,
        decoration: InputDecoration(hintText: widget.hint),
        onSubmitted: (_) => _submit(),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: Text(l.cancel),
        ),
        TextButton(
          key: const Key('confirm-create'),
          onPressed: _submit,
          child: Text(widget.confirm),
        ),
      ],
    );
  }
}
