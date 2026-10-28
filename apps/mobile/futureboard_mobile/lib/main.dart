// Futureboard Studio Mobile — Standalone Rust DSP & Flutter DAW Entrypoint.

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import 'engine/studio_controller.dart';
import 'panels/app_chrome.dart';
import 'panels/arrangement_panel.dart';
import 'panels/browser_panel.dart';
import 'panels/editor_panel.dart';
import 'panels/inspector_panel.dart';
import 'panels/mixer_panel.dart';
import 'panels/perform_panel.dart';
import 'panels/plugin_editor_panel.dart';
import 'panels/status_bar.dart';
import 'theme/fb_tokens.dart';

void main() {
  WidgetsFlutterBinding.ensureInitialized();
  SystemChrome.setPreferredOrientations([
    DeviceOrientation.landscapeLeft,
    DeviceOrientation.landscapeRight,
    DeviceOrientation.portraitUp,
  ]);
  runApp(const FutureboardStudioMobileApp());
}

class FutureboardStudioMobileApp extends StatefulWidget {
  final StudioController? controller;

  const FutureboardStudioMobileApp({super.key, this.controller});

  @override
  State<FutureboardStudioMobileApp> createState() =>
      _FutureboardStudioMobileAppState();
}

class _FutureboardStudioMobileAppState
    extends State<FutureboardStudioMobileApp> {
  late final StudioController _controller;
  late final bool _ownsController;

  @override
  void initState() {
    super.initState();
    if (widget.controller != null) {
      _controller = widget.controller!;
      _ownsController = false;
    } else {
      _controller = StudioController();
      _ownsController = true;
    }
  }

  @override
  void dispose() {
    if (_ownsController) {
      _controller.dispose();
    }
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: _controller,
      builder: (context, _) {
        final c = _controller.colors;
        return MaterialApp(
          title: 'Futureboard Studio Mobile',
          debugShowCheckedModeBanner: false,
          theme: ThemeData(
            brightness: c.isDark ? Brightness.dark : Brightness.light,
            scaffoldBackgroundColor: c.bgCanvas,
            fontFamily: FbTypography.fontSans,
            colorScheme: ColorScheme.fromSeed(
              seedColor: c.accentPrimary,
              brightness: c.isDark ? Brightness.dark : Brightness.light,
            ),
          ),
          home: Scaffold(
            backgroundColor: c.bgCanvas,
            body: SafeArea(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  // 1. Top App Chrome (Titlebar + Transport Cluster)
                  AppChrome(controller: _controller),

                  // 2. Center Studio Shell (Browser + Arrangement/Dock + Inspector)
                  Expanded(
                    child: LayoutBuilder(
                      builder: (context, constraints) {
                        final isWide = constraints.maxWidth >= 900;
                        return Row(
                          crossAxisAlignment: CrossAxisAlignment.stretch,
                          children: [
                            if (_controller.isBrowserOpen)
                              BrowserPanel(controller: _controller),
                            Expanded(
                              child: Column(
                                crossAxisAlignment: CrossAxisAlignment.stretch,
                                children: [
                                  // Top Half: Arrangement Timeline
                                  Expanded(
                                    flex: _controller.isBottomDockOpen ? 52 : 100,
                                    child: ArrangementPanel(
                                        controller: _controller),
                                  ),

                                  // Dock Tab Strip Header
                                  _buildBottomDockTabBar(c, isWide),

                                  // Bottom Half: Active Dock Panel
                                  if (_controller.isBottomDockOpen)
                                    Expanded(
                                      flex: 48,
                                      child: _buildActiveDockContent(),
                                    ),
                                ],
                              ),
                            ),
                            if (_controller.isInspectorOpen && isWide)
                              InspectorPanel(controller: _controller),
                          ],
                        );
                      },
                    ),
                  ),

                  // 3. Bottom Studio Status Bar
                  StatusBar(controller: _controller),
                ],
              ),
            ),
          ),
        );
      },
    );
  }

  Widget _buildBottomDockTabBar(FbColors c, bool isWide) {
    return Container(
      height: 28,
      padding: const EdgeInsets.symmetric(horizontal: FbSpace.s3),
      decoration: BoxDecoration(
        color: c.bgPrimary,
        border: Border(
          top: BorderSide(color: c.borderDefault, width: 1.0),
          bottom: BorderSide(color: c.borderSubtle, width: 1.0),
        ),
      ),
      child: SingleChildScrollView(
        scrollDirection: Axis.horizontal,
        child: Row(
          children: [
            _dockTabChip(
              label: 'Mixer',
              icon: LucideIcons.slidersVertical,
              tab: StudioDockTab.mixer,
              c: c,
            ),
            const SizedBox(width: 4),
            _dockTabChip(
              label: 'Piano Roll / Clip',
              icon: LucideIcons.piano,
              tab: StudioDockTab.pianoRoll,
              c: c,
            ),
            const SizedBox(width: 4),
            _dockTabChip(
              label: 'Plugin DSP Editor',
              icon: LucideIcons.slidersHorizontal,
              tab: StudioDockTab.pluginEditor,
              c: c,
            ),
            const SizedBox(width: 4),
            _dockTabChip(
              label: 'Perform Pads & Keys',
              icon: LucideIcons.keyboardMusic,
              tab: StudioDockTab.perform,
              c: c,
            ),
            if (!isWide) ...[
              const SizedBox(width: 4),
              _dockTabChip(
                label: '3D Spatial & Chain',
                icon: LucideIcons.compass,
                tab: StudioDockTab.inspector,
                c: c,
              ),
            ],
            const SizedBox(width: FbSpace.s4),
            InkWell(
              onTap: _controller.toggleBottomDock,
              child: Padding(
                padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 2),
                child: Row(
                  children: [
                    Icon(
                      _controller.isBottomDockOpen
                          ? LucideIcons.chevronDown
                          : LucideIcons.chevronUp,
                      size: 13,
                      color: c.textMuted,
                    ),
                    const SizedBox(width: 4),
                    Text(
                      _controller.isBottomDockOpen ? 'Hide Dock' : 'Show Dock',
                      style: FbTypography.micro(c),
                    ),
                  ],
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _dockTabChip({
    required String label,
    required IconData icon,
    required StudioDockTab tab,
    required FbColors c,
  }) {
    final active =
        _controller.isBottomDockOpen && _controller.activeDockTab == tab;

    return InkWell(
      onTap: () => _controller.setDockTab(tab),
      borderRadius: BorderRadius.circular(FbRadius.xs),
      child: Container(
        height: 22,
        padding: const EdgeInsets.symmetric(horizontal: 8),
        alignment: Alignment.center,
        decoration: BoxDecoration(
          color: active ? c.accentSoft : Colors.transparent,
          borderRadius: BorderRadius.circular(FbRadius.xs),
          border: Border.all(
            color: active ? c.accentPrimary : Colors.transparent,
          ),
        ),
        child: Row(
          children: [
            Icon(
              icon,
              size: 12,
              color: active ? c.accentPrimary : c.textSecondary,
            ),
            const SizedBox(width: 5),
            Text(
              label,
              style: TextStyle(
                fontFamily: FbTypography.fontSans,
                fontSize: 11.0,
                fontWeight: active ? FontWeight.w700 : FontWeight.w500,
                color: active ? c.accentPrimary : c.textSecondary,
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _buildActiveDockContent() {
    switch (_controller.activeDockTab) {
      case StudioDockTab.mixer:
        return MixerPanel(controller: _controller);
      case StudioDockTab.pianoRoll:
        return EditorPanel(controller: _controller);
      case StudioDockTab.pluginEditor:
        return PluginEditorPanel(controller: _controller);
      case StudioDockTab.perform:
        return PerformPanel(controller: _controller);
      case StudioDockTab.inspector:
        return InspectorPanel(controller: _controller);
    }
  }
}
