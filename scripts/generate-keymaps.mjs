/**
 * Generate the DAW keyboard shortcut profiles.
 *
 * Output: packages/keymaps/{ableton,cubase,fl_studio,pro_tools,studio_one}.json
 *
 * Each profile ships only the keys it changes. Studio layers every built-in
 * profile on the hand-authored, scoped `default.json` (see `KeymapManager`),
 * so a profile that also carried a copy of the menu manifest's shortcuts would
 * re-impose stale keys the default has since moved, and two commands would
 * end up sharing one key. A `null` takes the default's key away from a
 * command. `KeymapManager`'s `no_shipped_profile_has_conflicts` test checks
 * the result against the default.
 */
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const root = path.join(__dirname, "..");
const outDir = path.join(root, "packages/keymaps");

/**
 * Extra bindings that exist in the default keymap but are not present in
 * the menu manifest. These are commands that have keyboard shortcuts but
 * no corresponding menu entry (e.g., MIDI editor keys, tool shortcuts,
 * track state toggles, etc.).
 * @type {Record<string, string|null>}
 */
const DEFAULT_OVERRIDES = {
  "project:new-from-template": "Ctrl+Shift+N",
  "project:snapshot": "Ctrl+Alt+S",
  "file:import-audio": "Ctrl+I",
  "file:export-stems": "Ctrl+Shift+E",
  "edit:delete-backspace": "Backspace",
  "clip:split-at-playhead": "S",
  "clip:consolidate": "Ctrl+J",
  "timeline:toggle-snap": "Ctrl+G",
  "project:reveal-folder": "Ctrl+Alt+R",
  "extensions:manager": "Ctrl+Alt+N",
  "midi:duplicate-selected": "Ctrl+D",
  "midi:nudge-left": "ArrowLeft",
  "midi:nudge-right": "ArrowRight",
  "midi:transpose-up": "ArrowUp",
  "midi:transpose-down": "ArrowDown",
  "midi:transpose-octave-up": "Shift+ArrowUp",
  "midi:transpose-octave-down": "Shift+ArrowDown",
  "audio:create-crossfade": "X",
  "audio:bounce-in-place": "Ctrl+B",
  "audio:render-selection": "Ctrl+R",
  "window:minimize": "Ctrl+M",
  "window:toggle-fullscreen": "F11",
  "panel:toggle-device-panel": "Ctrl+4",
  "panel:toggle-midi-editor": "Ctrl+5",
  "panel:toggle-automation": "Ctrl+6",
  // Tools sit on the number row in toolbar order, like most DAWs.
  "tools:select-pointer": "1",
  "tools:select-pen": "2",
  "tools:select-cut": "3",
  "tools:select-glue": "4",
  "tools:select-mute": "5",
  "tools:select-time": "6",
  "tools:select-automation": "7",
  "tools:command-palette": "Ctrl+K",
  "tools:quick-search": "Ctrl+P",
  "tools:developer-tools": "Ctrl+Shift+I",
  "app:force-reload": "Ctrl+Shift+R",
  "track:arm": "Ctrl+Shift+B",
  "track:mute": "Ctrl+Shift+H",
  "track:solo": "Ctrl+Shift+L",
  "track:add": "T",
  "clip:rename": "F2",
  "clip:properties": "Ctrl+Shift+P",
  "midi:export-clip": "Ctrl+Alt+I",
  "file:export-audio": "Ctrl+Shift+X",
  "solfege:analyze-accent": "Ctrl+Alt+A",
  "solfege:apply-accent": "Ctrl+Alt+Shift+A",
  "solfege:analyze-accent-replace-all": "Ctrl+Alt+Shift+R",
  "automation:select-all-points": "Ctrl+Alt+Shift+P",
  "audio:stem-extractor": "Ctrl+Alt+Shift+E",
  "jam:open": "Ctrl+Alt+Shift+J",
  "midi:toggle-virtual-keyboard": "Ctrl+Alt+Shift+K",
  "window:chord-display": "Ctrl+Alt+Shift+C",
  "window:lyric-display": "Ctrl+Alt+Shift+D",
  "window:lyric-editor": "Ctrl+Alt+Shift+M",
  "floatingwindow:video-player": "Ctrl+Alt+Shift+V",
  "mixer:create-bus": "Ctrl+Alt+7",
  "mixer:reset-volume": "Ctrl+Alt+8",
  "mixer:reset-pan": "Ctrl+Alt+9",
  "midi:tool-select": "Ctrl+Alt+1",
  "midi:tool-draw": "Ctrl+Alt+2",
  "midi:tool-line": "Ctrl+Alt+3",
  "midi:velocity-increase": "Ctrl+Alt+Up",
  "midi:velocity-decrease": "Ctrl+Alt+Down",
  "midi:toggle-snap": "Ctrl+Alt+4",
  "midi:fit-notes": "Ctrl+Alt+5",
  "editor:open-bottom": "Ctrl+Alt+6",
  "floatingwindow:mixer": "Ctrl+Alt+F",
  "floatingwindow:routing-matrix": "Ctrl+Alt+G",
};

/**
 * DAW-specific overrides keyed by command id.
 * Values of `null` remove a default binding for that profile.
 * @type {Record<string, Record<string, string|null>>}
 */
const DAW_OVERRIDES = {
  ableton: {
    "transport:record": "F9",
    "transport:toggle-loop": "Ctrl+L",
    "transport:toggle-metronome": "O",
    // Ctrl+E splits; export is Ctrl+Shift+R below.
    "clip:split-at-playhead": "Ctrl+E",
    "file:export-arrangement": null,
    "midi:quantize": "Ctrl+U",
    "edit:deselect-all": "Ctrl+Shift+A",
    "file:export-audio": "Ctrl+Shift+R",
    // Insert Return Track; Live has no instrument-track key.
    "track:add-return": "Ctrl+Alt+T",
    "track:add-instrument": null,
    "panel:toggle-browser": "Ctrl+Alt+B",
    "tools:select-pen": "B",
    "tools:select-pointer": null,
    "tools:select-cut": null,
    "tools:select-glue": null,
    "tools:select-time": null,
    // A is Automation Arm; Live has no track-arm key.
    "tools:select-automation": "A",
    "track:arm": null,
    "timeline:toggle-snap": null,
    "marker:add": null,
    "audio:bounce-in-place": "Ctrl+J",
    "view:zoom-in": "+",
    "view:zoom-out": "-",
    "view:reset-zoom": null,
    "panel:toggle-mixer": null,
    "panel:toggle-inspector": null,
    "panel:toggle-device-panel": null,
    "panel:toggle-midi-editor": null,
    "panel:toggle-automation": null,
    "tools:quick-search": null,
  },
  cubase: {
    "transport:record": "Numpad*",
    "transport:toggle-loop": "Numpad/",
    "transport:toggle-metronome": "C",
    "transport:go-to-start": "Numpad.",
    "transport:rewind": "Shift+Numpad-",
    "transport:fast-forward": "Shift+Numpad+",
    "clip:split-at-playhead": "Alt+X",
    "tools:select-pointer": "1",
    "tools:select-pen": "8",
    "tools:select-cut": "3",
    "tools:select-glue": "4",
    "tools:select-time": "6",
    "tools:select-mute": "7",
    // Cubase has no automation tool; 7 went to Mute and A stays Arm.
    "tools:select-automation": null,
    "timeline:toggle-snap": null,
    "marker:add": null,
    // Ctrl+T is Add Track; Cubase has no separate audio-track key.
    "track:show-add-dialog": "Ctrl+T",
    "track:add-audio": null,
    "track:add-midi": "Ctrl+Shift+I",
    "audio:bounce-in-place": "Ctrl+Shift+B",
    "audio:render-selection": "Alt+Shift+R",
    "edit:deselect-all": "Ctrl+Shift+A",
    "midi:nudge-left": "Ctrl+ArrowLeft",
    "midi:nudge-right": "Ctrl+ArrowRight",
    "midi:transpose-up": "Shift+ArrowUp",
    "midi:transpose-down": "Shift+ArrowDown",
    "midi:transpose-octave-up": "Ctrl+Shift+ArrowUp",
    "midi:transpose-octave-down": "Ctrl+Shift+ArrowDown",
    "panel:toggle-browser": null,
    // F3 is the MixConsole; the performance meter gives the key up. (F2 is
    // the Transport Panel in Cubase, so the editor keeps its default key and
    // F2 stays Rename.)
    "panel:toggle-mixer": "F3",
    "view:toggle-perf-metrics": null,
    "panel:toggle-inspector": null,
    "panel:toggle-device-panel": null,
    "tools:command-palette": null,
  },
  fl_studio: {
    "transport:stop": "Ctrl+Space",
    // Ctrl+M is the metronome; FL has no minimize key.
    "transport:toggle-metronome": "Ctrl+M",
    "window:minimize": null,
    "transport:go-to-start": null,
    "transport:go-to-end": null,
    "transport:rewind": "Numpad/",
    "transport:fast-forward": "Numpad0",
    "clip:split-at-playhead": null,
    "tools:select-pointer": null,
    "tools:select-pen": null,
    "tools:select-cut": null,
    "tools:select-glue": null,
    "tools:select-time": null,
    "tools:select-automation": null,
    "marker:add": null,
    "track:show-add-dialog": null,
    "track:add-midi": null,
    "file:import-audio": "Ctrl+Shift+H",
    "panel:toggle-browser": null,
    "panel:toggle-mixer": "F9",
    "panel:toggle-midi-editor": "F7",
    "panel:toggle-device-panel": "F10",
    "tools:quick-search": "Alt+F8",
    "edit:duplicate": "Ctrl+B",
    "audio:bounce-in-place": "Ctrl+Shift+B",
    "view:zoom-in": "Ctrl+Up",
    "view:zoom-out": "Ctrl+Down",
    "view:reset-zoom": null,
    "tools:command-palette": null,
  },
  pro_tools: {
    // Pro Tools drives transport and edit modes from the numeric keypad, which
    // this keymap format cannot express (a `+`-separated accelerator cannot
    // carry a `Numpad+` base key). Only the bindings that survive on a laptop
    // keyboard are mapped here; the rest keep the Futureboard default.
    // Return (go to start) stays unmapped: a global key may not take the
    // song text editor's Enter; Home still goes to the start.
    "transport:record": "F12",
    "transport:toggle-loop": "Ctrl+Shift+L",
    "window:lyric-editor": null,
    // "Separate Clip at Selection" takes Ctrl+E from export-arrangement.
    "clip:split-at-playhead": "Ctrl+E",
    "file:export-arrangement": null,
    "edit:duplicate": "Ctrl+D",
    "edit:deselect-all": "Ctrl+Shift+A",
    // Edit tools row: F7 Selector, F8 Grabber, F10 Pencil. Pro Tools has no
    // cut/glue/automation tool, so those keep no binding in this profile.
    "tools:select-time": "F7",
    "tools:select-pointer": "F8",
    "tools:select-pen": "F10",
    "tools:select-cut": null,
    "tools:select-glue": null,
    "tools:select-automation": null,
    // Edit modes F1–F4; Grid mode is the snap equivalent.
    "timeline:toggle-snap": "F4",
    "window:audio-connections": null,
    // "New Tracks…" takes Ctrl+Shift+N from Futureboard's new-from-template.
    "track:show-add-dialog": "Ctrl+Shift+N",
    "project:new-from-template": null,
    "track:add-midi": null,
    "file:import-audio": "Ctrl+Shift+I",
    // "Bounce to Disk".
    "file:export-audio": "Ctrl+Alt+B",
    "song_text.add_both_at_playhead": null,
    // Event Operations ▸ Quantize.
    "midi:quantize": "Alt+0",
    "view:zoom-in": "Ctrl+]",
    "view:zoom-out": "Ctrl+[",
    "view:reset-zoom": null,
    // Ctrl+= toggles the Edit and Mix windows.
    "panel:toggle-mixer": "Ctrl+=",
    // Workspace browser.
    "panel:toggle-browser": "Alt+;",
    "marker:add": null,
    "audio:bounce-in-place": null,
    "audio:render-selection": null,
    "tools:command-palette": null,
    "tools:quick-search": null,
  },
  studio_one: {
    "transport:record": "Numpad*",
    "transport:stop": "Numpad0",
    "transport:toggle-loop": "Numpad/",
    "transport:go-to-start": "Numpad,",
    "transport:rewind": "Numpad-",
    "transport:fast-forward": "Numpad+",
    "clip:split-at-playhead": "Alt+X",
    "tools:select-pointer": "1",
    "tools:select-pen": "5",
    "tools:select-cut": "3",
    "tools:select-glue": null,
    "tools:select-mute": "6",
    "tools:select-time": "7",
    "tools:select-automation": "A",
    "track:show-add-dialog": "T",
    "track:add-midi": "Ctrl+Shift+T",
    "edit:duplicate": "D",
    "edit:deselect-all": "Ctrl+D",
    "file:export-audio": "Ctrl+E",
    "file:export-stems": "Ctrl+Shift+E",
    "audio:bounce-in-place": "Ctrl+B",
    "audio:render-selection": "Ctrl+R",
    "midi:nudge-left": "Alt+ArrowLeft",
    "midi:nudge-right": "Alt+ArrowRight",
    "midi:transpose-up": "Shift+ArrowUp",
    "midi:transpose-down": "Shift+ArrowDown",
    "midi:transpose-octave-up": "Ctrl+Shift+ArrowUp",
    "midi:transpose-octave-down": "Ctrl+Shift+ArrowDown",
    "timeline:toggle-snap": "N",
    "marker:add": null,
    "panel:toggle-browser": "F5",
    "panel:toggle-mixer": "F3",
    "panel:toggle-inspector": "F4",
    "panel:toggle-midi-editor": "F2",
    "tools:command-palette": null,
  },
};

/** @type {{ id: string; label: string; description: string; overrides?: Record<string, string|null> }[]} */
// `default.json` is not generated: it is the hand-authored, scoped (version 2)
// base every other profile layers on — global keys plus one block per editor.
// Regenerating it from the flat menu manifest would drop the scopes.
// `DEFAULT_OVERRIDES` stays only as history of how the old flat default was made.
const PROFILES = [
  {
    id: "ableton",
    label: "Ableton Live",
    description: "Shortcuts aligned with Ableton Live defaults where commands overlap.",
    overrides: DAW_OVERRIDES.ableton,
  },
  {
    id: "cubase",
    label: "Cubase",
    description: "Shortcuts aligned with Steinberg Cubase defaults where commands overlap.",
    overrides: DAW_OVERRIDES.cubase,
  },
  {
    id: "fl_studio",
    label: "FL Studio",
    description: "Shortcuts aligned with FL Studio defaults where commands overlap.",
    overrides: DAW_OVERRIDES.fl_studio,
  },
  {
    id: "pro_tools",
    label: "Pro Tools",
    description: "Shortcuts aligned with Avid Pro Tools defaults where commands overlap.",
    overrides: DAW_OVERRIDES.pro_tools,
  },
  {
    id: "studio_one",
    label: "Studio One",
    description: "Shortcuts aligned with PreSonus Studio One defaults where commands overlap.",
    overrides: DAW_OVERRIDES.studio_one,
  },
];

function main() {
  fs.mkdirSync(outDir, { recursive: true });

  for (const profile of PROFILES) {
    const bindings = Object.fromEntries(
      Object.entries(profile.overrides ?? {}).sort(([a], [b]) => a.localeCompare(b)),
    );
    const payload = {
      version: 1,
      id: profile.id,
      label: profile.label,
      description: profile.description,
      extends: "default",
      bindings,
    };
    const outPath = path.join(outDir, `${profile.id}.json`);
    fs.writeFileSync(outPath, `${JSON.stringify(payload, null, 2)}
`, "utf8");
    console.log(`wrote ${outPath} (${Object.keys(bindings).length} bindings)`);
  }
}

main();
