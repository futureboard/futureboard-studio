export type AppMenuItem =
  | {
      type?: "item";
      id: string;
      label: string;
      accelerator?: string;
      icon?: string;
      dot?: string;       // CSS color for a small dot prefix (e.g. track colors)
      enabled?: boolean;
      checked?: boolean;
      danger?: boolean;
      role?: string;
      action?: string;
      description?: string;
    }
  | {
      type: "separator";
      id: string;
    }
  | {
      type: "submenu";
      id: string;
      label: string;
      icon?: string;
      enabled?: boolean;
      children: AppMenuItem[];
    };

export type AppMenuGroup = {
  id: string;
  label: string;
  children: AppMenuItem[];
};

export const APP_MENUS: AppMenuGroup[] = [
  {
    id: "file",
    label: "File",
    children: [
      {
        id: "file.new_project",
        label: "New Project",
        accelerator: "Ctrl+N",
        icon: "file-plus",
        action: "project:new",
      },
      {
        id: "file.open_project",
        label: "Open Project...",
        accelerator: "Ctrl+O",
        icon: "folder-open",
        action: "project:open",
      },
      {
        id: "file.open_recent",
        type: "submenu",
        label: "Open Recent",
        icon: "history",
        children: [
          {
            id: "file.open_recent.empty",
            label: "No Recent Projects",
            enabled: false,
            action: "noop",
          },
          {
            id: "file.open_recent.clear",
            label: "Clear Recent Projects",
            action: "project:recent-clear",
          },
        ],
      },
      {
        type: "separator",
        id: "file.sep.save",
      },
      {
        id: "file.save",
        label: "Save",
        accelerator: "Ctrl+S",
        icon: "save",
        action: "project:save",
      },
      {
        id: "file.save_as",
        label: "Save As...",
        accelerator: "Ctrl+Shift+S",
        icon: "save-all",
        action: "project:save-as",
      },
      {
        id: "file.save_copy",
        label: "Save a Copy...",
        accelerator: "Ctrl+Alt+Shift+S",
        icon: "copy",
        action: "project:save-copy",
      },
      {
        type: "separator",
        id: "file.sep.export",
      },
      {
        id: "file.export_arrangement",
        label: "Export Arrangement...",
        accelerator: "Ctrl+E",
        icon: "download",
        action: "file:export-arrangement",
      },
      {
        id: "file.export_midi",
        label: "Export MIDI File...",
        accelerator: "Ctrl+Alt+E",
        icon: "download",
        action: "file:export-midi",
      },
      {
        type: "separator",
        id: "file.sep.close",
      },
      {
        id: "file.close_project",
        label: "Close Project",
        accelerator: "Ctrl+W",
        icon: "x",
        action: "project:close",
      },
      {
        id: "file.quit",
        label: "Quit",
        accelerator: "Alt+F4",
        icon: "power",
        role: "quit",
        action: "app:quit",
      },
    ],
  },

  {
    id: "edit",
    label: "Edit",
    children: [
      {
        id: "edit.undo",
        label: "Undo",
        accelerator: "Ctrl+Z",
        icon: "undo-2",
        action: "edit:undo",
      },
      {
        id: "edit.redo",
        label: "Redo",
        accelerator: "Ctrl+Y",
        icon: "redo-2",
        action: "edit:redo",
      },
      {
        type: "separator",
        id: "edit.sep.clipboard",
      },
      {
        id: "edit.cut",
        label: "Cut",
        accelerator: "Ctrl+X",
        icon: "scissors",
        action: "edit:cut",
      },
      {
        id: "edit.copy",
        label: "Copy",
        accelerator: "Ctrl+C",
        icon: "copy",
        action: "edit:copy",
      },
      {
        id: "edit.paste",
        label: "Paste",
        accelerator: "Ctrl+V",
        icon: "clipboard",
        action: "edit:paste",
      },
      {
        id: "edit.duplicate",
        label: "Duplicate",
        accelerator: "Ctrl+D",
        icon: "copy-plus",
        action: "edit:duplicate",
      },
      {
        id: "edit.delete",
        label: "Delete",
        accelerator: "Delete",
        icon: "trash-2",
        danger: true,
        action: "edit:delete",
      },
      {
        type: "separator",
        id: "edit.sep.select",
      },
      {
        id: "edit.select_all",
        label: "Select All",
        accelerator: "Ctrl+A",
        icon: "scan",
        action: "edit:select-all",
      },
      {
        id: "edit.deselect_all",
        label: "Deselect All",
        accelerator: "Esc",
        icon: "scan-x",
        action: "edit:deselect-all",
      },
      {
        type: "separator",
        id: "edit.sep.preferences",
      },
      {
        id: "edit.preferences",
        label: "Preferences...",
        accelerator: "Ctrl+,",
        icon: "settings",
        action: "app:preferences",
      },
    ],
  },

  {
    id: "view",
    label: "View",
    children: [
      {
        id: "view.developer",
        type: "submenu",
        label: "Developer",
        children: [
          {
            id: "view.developer.perf_metrics",
            label: "Show Performance Metrics",
            checked: false,
            action: "view:toggle-perf-metrics",
            description: "Compact FPS and frame time in the status bar",
          },
          {
            id: "view.developer.perf_overlay",
            label: "Toggle Performance Overlay",
            checked: false,
            action: "view:toggle-perf-overlay",
            description: "Verbose real-time performance overlay",
          },
        ],
      },
    ],
  },

  {
    id: "midi",
    label: "MIDI",
    children: [
      {
        id: "midi.open_editor",
        label: "Open MIDI Editor",
        accelerator: "Ctrl+Shift+M",
        icon: "keyboard-music",
        action: "midi:open-editor",
      },
      {
        id: "midi.virtual_keyboard",
        label: "Virtual Keyboard",
        accelerator: "Alt+K",
        icon: "keyboard-music",
        action: "view:toggle-virtual-keyboard",
      },
      {
        type: "separator",
        id: "midi.sep.editor",
      },
      {
        id: "midi.select_all",
        label: "Select All Notes",
        accelerator: "Ctrl+A",
        icon: "scan",
        action: "midi:select-all",
      },
      {
        id: "midi.delete_selected",
        label: "Delete Selected Notes",
        accelerator: "Delete",
        icon: "trash-2",
        danger: true,
        action: "midi:delete-selected",
      },
      {
        id: "midi.quantize",
        label: "Quantize",
        accelerator: "Q",
        icon: "align-start-vertical",
        action: "midi:quantize",
      },
      {
        id: "midi.fit_notes",
        label: "Fit Notes",
        icon: "maximize-2",
        accelerator: "Ctrl+Alt+5",
        action: "midi:fit-notes",
      },
      {
        type: "separator",
        id: "midi.sep.accent",
      },
      {
        id: "midi.analyze_accent",
        label: "Analyze Accent",
        icon: "wand-sparkles",
        accelerator: "Ctrl+Alt+A",
        action: "solfege:analyze-accent",
        description:
          "Estimate how strongly each note should be emphasised, keeping accents edited by hand",
      },
      {
        id: "midi.analyze_accent_replace",
        label: "Analyze Accent (Replace All)",
        icon: "wand-sparkles",
        accelerator: "Ctrl+Alt+Shift+R",
        action: "solfege:analyze-accent-replace-all",
        description: "Re-analyse every note, discarding accents edited by hand",
      },
      {
        id: "midi.apply_accent",
        label: "Apply Accent to Performance",
        icon: "audio-waveform",
        accelerator: "Ctrl+Alt+Shift+A",
        action: "solfege:apply-accent",
        description:
          "Write the analysed accents into note timing, velocity and the Dynamics lane",
      },
    ],
  },

  {
    id: "project",
    label: "Project",
    children: [
      {
        id: "project.settings",
        label: "Project Settings...",
        accelerator: "Ctrl+.",
        icon: "settings-2",
        action: "project:settings",
      },
      {
        type: "separator",
        id: "project.sep.tracks",
      },
      {
        id: "project.add_audio_track",
        label: "Add Audio Track",
        accelerator: "Ctrl+Shift+A",
        icon: "mic",
        action: "track:add-audio",
      },
      {
        id: "project.add_midi_track",
        label: "Add MIDI Track",
        accelerator: "Ctrl+Shift+T",
        icon: "piano",
        action: "track:add-midi",
      },
      {
        id: "project.add_instrument_track",
        label: "Add Instrument Track",
        icon: "keyboard-music",
        accelerator: "Ctrl+Shift+G",
        action: "track:add-instrument",
      },
      {
        id: "project.add_plugin_track",
        label: "Add Plugin Track",
        icon: "cpu",
        action: "track:add-plugin",
      },
      {
        id: "project.add_bus_track",
        label: "Add Bus Track",
        icon: "route",
        accelerator: "Ctrl+Shift+J",
        action: "track:add-bus",
      },
      {
        id: "project.add_return_track",
        label: "Add Return Track",
        icon: "corner-down-left",
        accelerator: "Ctrl+Shift+K",
        action: "track:add-return",
      },
      {
        type: "separator",
        id: "project.sep.track_actions",
      },
      {
        id: "project.delete_track",
        label: "Delete Selected Track",
        accelerator: "Ctrl+Shift+Delete",
        icon: "trash-2",
        danger: true,
        action: "track:delete",
      },
    ],
  },

  {
    id: "audio",
    label: "Audio",
    children: [
      {
        id: "audio.play_pause",
        label: "Play / Pause",
        accelerator: "Space",
        icon: "play",
        action: "transport:play-pause",
      },
      {
        id: "audio.stop",
        label: "Stop",
        accelerator: "Shift+Space",
        icon: "square",
        action: "transport:stop",
      },
      {
        id: "audio.record",
        label: "Record",
        accelerator: "R",
        icon: "circle",
        action: "transport:record",
      },
      {
        id: "audio.loop",
        label: "Loop",
        accelerator: "L",
        icon: "repeat",
        checked: false,
        action: "transport:toggle-loop",
      },
      {
        id: "audio.metronome",
        label: "Metronome",
        accelerator: "K",
        icon: "timer",
        checked: false,
        action: "transport:toggle-metronome",
      },
      {
        id: "audio.tap_tempo",
        type: "submenu",
        label: "Tap Tempo",
        icon: "timer",
        children: [
          {
            id: "audio.tap_tempo.tap",
            label: "Tap",
            accelerator: "Shift+B",
            icon: "timer",
            action: "tempo:tap",
            description: "Register a tap; updates project BPM from the second tap onward",
          },
          {
            id: "audio.tap_tempo.reset",
            label: "Reset Tap Session",
            icon: "rotate-ccw",
            action: "tempo:reset-tap",
          },
          {
            type: "separator",
            id: "audio.tap_tempo.sep",
          },
          {
            id: "audio.tap_tempo.add_marker",
            label: "Add Current Tempo at Playhead",
            icon: "map-pin",
            action: "tempo:add-tap-marker",
          },
        ],
      },
      {
        type: "separator",
        id: "audio.sep.navigation",
      },
      {
        id: "audio.go_to_start",
        label: "Go to Start",
        accelerator: "Home",
        icon: "step-back",
        action: "transport:go-to-start",
      },
      {
        id: "audio.go_to_end",
        label: "Go to End",
        accelerator: "End",
        icon: "step-forward",
        action: "transport:go-to-end",
      },
      {
        id: "audio.rewind",
        label: "Rewind",
        accelerator: "Alt+Left",
        icon: "rewind",
        action: "transport:rewind",
      },
      {
        id: "audio.fast_forward",
        label: "Fast Forward",
        accelerator: "Alt+Right",
        icon: "fast-forward",
        action: "transport:fast-forward",
      },
      {
        type: "separator",
        id: "audio.sep.plugins",
      },
      {
        id: "audio.insert_plugin",
        label: "Insert Plug-in...",
        accelerator: "Ctrl+Shift+U",
        icon: "plug",
        action: "plugins:insert",
      },
      {
        id: "audio.plugin_manager",
        label: "Audio Plug-in Manager...",
        accelerator: "Ctrl+Alt+M",
        icon: "blocks",
        action: "plugins:manager",
      },
      {
        id: "audio.plugin_scanner",
        label: "Audio Plug-in Scanner...",
        icon: "scan",
        accelerator: "Ctrl+Alt+U",
        action: "plugins:scan",
        description: "Scan for installed VST3 and CLAP plug-ins",
      },
      {
        type: "separator",
        id: "audio.sep.tools",
      },
      {
        id: "audio.stem_extractor",
        label: "Stem Extractor...",
        icon: "audio-lines",
        accelerator: "Ctrl+Alt+Shift+E",
        action: "audio:stem-extractor",
        description: "Separate a mix into stems with MDX-NET (CPU/GPU)",
      },
      {
        type: "separator",
        id: "audio.sep.routing",
      },
      {
        id: "audio.connections",
        label: "Audio Connections...",
        icon: "route",
        accelerator: "Ctrl+Alt+O",
        action: "window:audio-connections",
        description: "Edit the project's logical audio input and output buses",
      },
    ],
  },

  {
    id: "automation",
    label: "Automation",
    children: [
      {
        id: "automation.select_all_points",
        label: "Select All Points",
        accelerator: "Ctrl+A",
        icon: "scan",
        action: "automation:select-all-points",
      },
      {
        id: "automation.delete_selected_points",
        label: "Delete Selected Points",
        accelerator: "Delete",
        icon: "trash-2",
        danger: true,
        action: "automation:delete-selected-points",
      },
      {
        id: "automation.clear_selection",
        label: "Clear Selection",
        accelerator: "Ctrl+Shift+D",
        icon: "scan-x",
        action: "automation:clear-selection",
      },
      {
        type: "separator",
        id: "automation.sep.track",
      },
      {
        id: "automation.toggle_mode",
        label: "Toggle Automation Mode",
        accelerator: "Ctrl+Shift+O",
        icon: "activity",
        action: "automation:toggle-mode",
      },
      {
        id: "automation.cycle_target",
        label: "Cycle Automation Target",
        accelerator: "Ctrl+Shift+Y",
        icon: "workflow",
        action: "automation:cycle-target",
      },
    ],
  },

  {
    id: "window",
    label: "Window",
    children: [
      {
        id: "window.show_browser",
        label: "Show Browser",
        accelerator: "Ctrl+1",
        icon: "folder",
        checked: true,
        action: "panel:toggle-browser",
      },
      {
        id: "window.show_inspector",
        label: "Show Inspector",
        accelerator: "Ctrl+2",
        icon: "panel-right",
        checked: true,
        action: "panel:toggle-inspector",
      },
      {
        id: "window.show_mixer",
        label: "Show Mixer",
        accelerator: "Ctrl+3",
        icon: "sliders-horizontal",
        checked: true,
        action: "panel:toggle-mixer",
      },
      {
        id: "window.show_bottom_panel",
        label: "Show Bottom Panel",
        accelerator: "Ctrl+7",
        icon: "panel-bottom",
        checked: true,
        action: "panel:toggle-bottom",
        description:
          "Show or hide the bottom dock itself, whichever tab is in it",
      },
      {
        id: "window.float_mixer",
        label: "Open Mixer in Window",
        icon: "external-link",
        accelerator: "Ctrl+Alt+F",
        action: "floatingwindow:mixer",
      },
      {
        id: "window.routing_matrix",
        label: "Routing Matrix",
        icon: "route",
        accelerator: "Ctrl+Alt+G",
        action: "floatingwindow:routing-matrix",
        description:
          "Cross-point grid of every track against Main, the buses and the returns, for toggling sends",
      },
      {
        id: "window.video_player",
        label: "Video Player",
        icon: "film",
        accelerator: "Ctrl+Alt+V",
        action: "window:video-player",
        description:
          "Preview the Video track's reference video against the playhead",
      },
      {
        id: "window.audio_jam",
        label: "Audio Jam",
        icon: "radio",
        accelerator: "Ctrl+Alt+J",
        action: "window:audio-jam",
        description:
          "Play with other Futureboard accounts over the network, and route their streams to tracks",
      },
      {
        type: "separator",
        id: "window.sep.clocks",
      },
      {
        id: "window.big_clock",
        label: "Big Clock",
        icon: "clock",
        accelerator: "Ctrl+Alt+K",
        action: "window:big-clock",
        description:
          "The playhead in bars and beats, large enough to read from across the room",
      },
      {
        id: "window.timecode",
        label: "Timecode",
        icon: "timer",
        accelerator: "Ctrl+Alt+T",
        action: "window:timecode",
        description:
          "The same clock led by SMPTE timecode, at 24, 25, 29.97 DF or 30 fps",
      },
      {
        id: "window.performance",
        label: "Performance Monitor",
        icon: "gauge",
        accelerator: "Ctrl+Alt+P",
        action: "window:performance",
        description:
          "Audio engine latency and PDC, plus every CPU core, memory and local drive",
      },
      {
        type: "separator",
        id: "window.sep.extensions",
      },
      {
        id: "window.extensions",
        label: "Extensions...",
        icon: "blocks",
        accelerator: "Ctrl+Alt+X",
        action: "window:extensions",
        description:
          "Browse and install community themes and extensions from the registry",
      },
      {
        type: "separator",
        id: "window.sep.song_text",
      },
      {
        id: "window.chord_display_panel",
        label: "Chord Display in Right Dock",
        icon: "music",
        accelerator: "Ctrl+8",
        action: "panel:show-chord-display",
      },
      {
        id: "window.lyric_display_panel",
        label: "Lyric Display in Right Dock",
        icon: "music",
        accelerator: "Ctrl+9",
        action: "panel:show-lyric-display",
      },
      {
        id: "window.lyric_editor_panel",
        label: "Lyric Editor in Right Dock",
        icon: "pencil",
        action: "panel:show-lyric-editor",
      },
      {
        id: "window.song_text_commands",
        type: "submenu",
        label: "Song Text",
        icon: "music",
        children: [
          {
            id: "song_text.add_chord_at_playhead",
            label: "Add Chord at Playhead",
            accelerator: "Ctrl+Alt+C",
            action: "song_text.add_chord_at_playhead",
          },
          {
            id: "song_text.add_lyric_at_playhead",
            label: "Add Lyric at Playhead",
            accelerator: "Ctrl+Alt+L",
            action: "song_text.add_lyric_at_playhead",
          },
          {
            id: "song_text.add_both_at_playhead",
            label: "Add Chord and Lyric at Playhead",
            accelerator: "Ctrl+Alt+B",
            action: "song_text.add_both_at_playhead",
          },
          {
            id: "song_text.commit",
            label: "Commit Song Text Edit",
            action: "song_text.commit",
          },
          {
            id: "song_text.commit_next_grid",
            label: "Commit and Advance to Next Grid",
            action: "song_text.commit_next_grid",
          },
          {
            id: "song_text.commit_next_beat",
            label: "Commit and Advance to Next Beat",
            action: "song_text.commit_next_beat",
          },
          {
            id: "song_text.commit_next_bar",
            label: "Commit and Advance to Next Bar",
            action: "song_text.commit_next_bar",
          },
          {
            id: "song_text.previous_event",
            label: "Previous Song Text Event",
            action: "song_text.previous_event",
          },
          {
            id: "song_text.next_event",
            label: "Next Song Text Event",
            action: "song_text.next_event",
          },
          {
            id: "song_text.move_to_playhead",
            label: "Move Song Text to Playhead",
            action: "song_text.move_to_playhead",
          },
          {
            id: "song_text.delete_selected",
            label: "Delete Selected Song Text",
            action: "song_text.delete_selected",
          },
        ],
      },
      {
        id: "window.chord_display",
        label: "Open Chord Display Window",
        icon: "external-link",
        accelerator: "Ctrl+Alt+Shift+C",
        action: "window:chord-display",
      },
      {
        id: "window.lyric_display",
        label: "Open Lyric Display Window",
        icon: "external-link",
        accelerator: "Ctrl+Alt+Shift+D",
        action: "window:lyric-display",
      },
      {
        id: "window.lyric_editor",
        label: "Open Lyric Editor Window",
        icon: "external-link",
        accelerator: "Ctrl+Alt+Shift+M",
        action: "window:lyric-editor",
      },
      {
        type: "separator",
        id: "window.sep.zoom",
      },
      {
        id: "window.zoom_in",
        label: "Zoom In",
        accelerator: "Ctrl+=",
        icon: "zoom-in",
        action: "view:zoom-in",
      },
      {
        id: "window.zoom_out",
        label: "Zoom Out",
        accelerator: "Ctrl+-",
        icon: "zoom-out",
        action: "view:zoom-out",
      },
      {
        id: "window.reset_zoom",
        label: "Reset Zoom",
        accelerator: "Ctrl+0",
        icon: "scan",
        action: "view:reset-zoom",
      },
    ],
  },

  {
    id: "tools",
    label: "Tools",
    children: [
      {
        id: "tools.select_pointer",
        label: "Pointer Tool",
        accelerator: "V",
        icon: "pointer",
        action: "tools:select-pointer",
      },
      {
        id: "tools.select_pen",
        label: "Pen Tool",
        accelerator: "P",
        icon: "pen",
        action: "tools:select-pen",
      },
      {
        id: "tools.select_cut",
        label: "Cut Tool",
        accelerator: "C",
        icon: "scissors",
        action: "tools:select-cut",
      },
      {
        id: "tools.select_glue",
        label: "Glue Tool",
        accelerator: "G",
        icon: "combine",
        action: "tools:select-glue",
      },
      {
        id: "tools.select_time",
        label: "Time Tool",
        accelerator: "T",
        icon: "move-horizontal",
        action: "tools:select-time",
      },
      {
        id: "tools.select_automation",
        label: "Automation Tool",
        accelerator: "A",
        icon: "activity",
        action: "tools:select-automation",
      },
    ],
  },

  {
    id: "help",
    label: "Help",
    children: [
      {
        id: "help.quick_start",
        label: "Quick Start Guide",
        icon: "book-open",
        action: "help:quick-start",
      },
      {
        id: "help.documentation",
        label: "Documentation",
        accelerator: "F1",
        icon: "book",
        action: "help:documentation",
      },
      {
        type: "separator",
        id: "help.sep.keymaps",
      },
      {
        id: "help.keyboard_shortcuts",
        label: "Keyboard Shortcuts & Keymaps...",
        accelerator: "Ctrl+/",
        icon: "keyboard",
        action: "help:keyboard-shortcuts",
        description: "View and edit keyboard shortcuts and keymap profiles",
      },
      {
        type: "separator",
        id: "help.sep.updates",
      },
      {
        id: "help.release_notes",
        label: "Release Notes",
        icon: "newspaper",
        action: "help:release-notes",
      },
      {
        id: "help.roadmap",
        label: "Roadmap",
        icon: "map",
        action: "help:roadmap",
      },
      {
        type: "separator",
        id: "help.sep.community",
      },
      {
        id: "help.github",
        label: "GitHub Repository",
        icon: "brand-github",
        action: "help:github",
      },
      {
        id: "help.community",
        label: "Community",
        icon: "users",
        action: "help:community",
      },
      {
        id: "help.report_issue",
        label: "Report an Issue",
        icon: "bug",
        action: "help:report-issue",
      },
      {
        id: "help.request_feature",
        label: "Request a Feature",
        icon: "lightbulb",
        action: "help:request-feature",
      },
      {
        type: "separator",
        id: "help.sep.diagnostics",
      },
      {
        id: "help.diagnostics",
        label: "Diagnostics",
        icon: "activity",
        action: "help:diagnostics",
        description: "Open performance and system diagnostics",
      },
      {
        id: "help.copy_system_info",
        label: "Copy System Info",
        icon: "clipboard-copy",
        action: "help:copy-system-info",
      },
      {
        type: "separator",
        id: "help.sep.app",
      },
      {
        id: "help.check_for_updates",
        label: "Check for Updates...",
        icon: "download",
        action: "app:check-for-updates",
      },
      {
        id: "help.about",
        label: "About Futureboard Studio",
        icon: "info",
        action: "app:about",
      },
    ],
  },
];
