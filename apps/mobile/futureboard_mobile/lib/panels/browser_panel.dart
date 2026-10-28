// Futureboard Studio Browser Panel: Instruments, Effects, Samples & Presets.

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../engine/studio_controller.dart';
import '../models/studio_models.dart';
import '../theme/fb_tokens.dart';
import '../widgets/fb_controls.dart';

class BrowserPanel extends StatefulWidget {
  final StudioController controller;

  const BrowserPanel({super.key, required this.controller});

  @override
  State<BrowserPanel> createState() => _BrowserPanelState();
}

class _BrowserPanelState extends State<BrowserPanel> {
  int _tabIndex = 0; // 0 = All, 1 = Instruments, 2 = Effects
  String _searchQuery = '';

  @override
  Widget build(BuildContext context) {
    final c = widget.controller.colors;
    final items = widget.controller.catalog.where((p) {
      if (_tabIndex == 1 && !p.isInstrument) return false;
      if (_tabIndex == 2 && p.isInstrument) return false;
      if (_searchQuery.isNotEmpty) {
        final q = _searchQuery.toLowerCase();
        return p.name.toLowerCase().contains(q) ||
            p.category.toLowerCase().contains(q) ||
            p.description.toLowerCase().contains(q);
      }
      return true;
    }).toList();

    return Container(
      width: 268,
      decoration: BoxDecoration(
        color: c.bgPrimary,
        border: Border(
          right: BorderSide(color: c.borderDefault, width: 1.0),
        ),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          // Header
          Container(
            height: 32,
            padding: const EdgeInsets.symmetric(horizontal: FbSpace.s3),
            decoration: BoxDecoration(
              color: c.bgSecondary,
              border: Border(
                bottom: BorderSide(color: c.borderSubtle, width: 1.0),
              ),
            ),
            child: Row(
              children: [
                Icon(LucideIcons.folderKanban, size: 13, color: c.accentPrimary),
                const SizedBox(width: 6),
                Text('STUDIO BROWSER', style: FbTypography.sectionHeader(c)),
                const Spacer(),
                Text(
                  '${items.length} DSP',
                  style: FbTypography.monoReadout(c, size: 10, color: c.textMuted),
                ),
              ],
            ),
          ),

          // Filter Tabs + Search Box
          Padding(
            padding: const EdgeInsets.all(FbSpace.s2 + 2),
            child: Column(
              children: [
                Row(
                  children: [
                    _tabChip('All (27)', 0, c),
                    const SizedBox(width: 4),
                    _tabChip('Inst (6)', 1, c),
                    const SizedBox(width: 4),
                    _tabChip('FX (21)', 2, c),
                  ],
                ),
                const SizedBox(height: 6),
                SizedBox(
                  height: 28,
                  child: TextField(
                    onChanged: (v) => setState(() => _searchQuery = v),
                    style: FbTypography.body(c),
                    decoration: InputDecoration(
                      hintText: 'Search Rust DSP plugins...',
                      hintStyle: FbTypography.micro(c),
                      prefixIcon:
                          Icon(LucideIcons.search, size: 13, color: c.textMuted),
                      prefixIconConstraints: const BoxConstraints(minWidth: 28),
                      filled: true,
                      fillColor: c.bgSunken,
                      contentPadding: const EdgeInsets.symmetric(
                          horizontal: 8, vertical: 0),
                      enabledBorder: OutlineInputBorder(
                        borderRadius: BorderRadius.circular(FbRadius.sm),
                        borderSide: BorderSide(color: c.borderDefault),
                      ),
                      focusedBorder: OutlineInputBorder(
                        borderRadius: BorderRadius.circular(FbRadius.sm),
                        borderSide: BorderSide(color: c.borderFocus),
                      ),
                    ),
                  ),
                ),
              ],
            ),
          ),

          // Plugin List
          Expanded(
            child: ListView.separated(
              padding: const EdgeInsets.symmetric(
                  horizontal: FbSpace.s2 + 2, vertical: 2),
              itemCount: items.length,
              separatorBuilder: (_, _) => const SizedBox(height: 4),
              itemBuilder: (context, idx) {
                final plugin = items[idx];
                return _buildPluginCard(plugin, c);
              },
            ),
          ),

          // Quick Track Creation Footer
          Container(
            padding: const EdgeInsets.all(FbSpace.s2 + 2),
            decoration: BoxDecoration(
              color: c.bgSecondary,
              border: Border(
                top: BorderSide(color: c.borderSubtle, width: 1.0),
              ),
            ),
            child: Wrap(
              spacing: 4,
              runSpacing: 4,
              children: [
                FbButton(
                  label: '+ Synth',
                  icon: LucideIcons.piano,
                  compact: true,
                  colors: c,
                  onTap: () => widget.controller.addTrack(
                    'Synth Track',
                    TrackKind.instrument,
                    instrumentId: 'wrapsynth',
                  ),
                ),
                FbButton(
                  label: '+ Drums',
                  icon: LucideIcons.drum,
                  compact: true,
                  colors: c,
                  onTap: () => widget.controller.addTrack(
                    'Drum Sampler',
                    TrackKind.drumKit,
                    instrumentId: 'drumsampler',
                  ),
                ),
                FbButton(
                  label: '+ Audio',
                  icon: LucideIcons.mic,
                  compact: true,
                  colors: c,
                  onTap: () => widget.controller.addTrack(
                    'Audio Track',
                    TrackKind.audio,
                  ),
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }

  Widget _tabChip(String text, int index, FbColors c) {
    final active = _tabIndex == index;
    return Expanded(
      child: InkWell(
        onTap: () => setState(() => _tabIndex = index),
        borderRadius: BorderRadius.circular(FbRadius.xs),
        child: Container(
          height: 24,
          alignment: Alignment.center,
          decoration: BoxDecoration(
            color: active ? c.accentSoft : c.bgTertiary,
            borderRadius: BorderRadius.circular(FbRadius.xs),
            border: Border.all(
              color: active ? c.accentPrimary : c.borderDefault,
            ),
          ),
          child: Text(
            text,
            style: TextStyle(
              fontFamily: FbTypography.fontSans,
              fontSize: 10.5,
              fontWeight: active ? FontWeight.w700 : FontWeight.w500,
              color: active ? c.accentPrimary : c.textSecondary,
            ),
          ),
        ),
      ),
    );
  }

  Widget _buildPluginCard(CatalogPluginModel plugin, FbColors c) {
    final badgeColor =
        plugin.isInstrument ? const Color(0xFF9B6BFF) : c.accentPrimary;

    return Container(
      padding: const EdgeInsets.all(FbSpace.s2 + 2),
      decoration: BoxDecoration(
        color: c.bgSecondary,
        borderRadius: BorderRadius.circular(FbRadius.sm),
        border: Border.all(color: c.borderSubtle),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Container(
                padding:
                    const EdgeInsets.symmetric(horizontal: 5, vertical: 1.5),
                decoration: BoxDecoration(
                  color: badgeColor.withValues(alpha: 0.18),
                  borderRadius: BorderRadius.circular(FbRadius.xs),
                  border: Border.all(
                      color: badgeColor.withValues(alpha: 0.55), width: 1.0),
                ),
                child: Text(
                  plugin.isInstrument ? 'INST' : plugin.category.toUpperCase(),
                  style: TextStyle(
                    fontFamily: FbTypography.fontMono,
                    fontSize: 9.0,
                    fontWeight: FontWeight.w700,
                    color: badgeColor,
                  ),
                ),
              ),
              const SizedBox(width: 6),
              Expanded(
                child: Text(
                  plugin.name,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: FbTypography.body(c),
                ),
              ),
              FbButton(
                label: 'Load',
                compact: true,
                colors: c,
                onTap: () => widget.controller.loadPluginFromBrowser(plugin),
              ),
            ],
          ),
          const SizedBox(height: 3),
          Text(
            plugin.description,
            maxLines: 2,
            overflow: TextOverflow.ellipsis,
            style: FbTypography.micro(c),
          ),
        ],
      ),
    );
  }
}
