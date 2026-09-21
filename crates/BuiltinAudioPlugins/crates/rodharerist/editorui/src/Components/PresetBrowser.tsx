// Compact tabbed sidebar: PRESET | IR | NAM A2, backed by real files under
// Documents/Futureboard Studio/Rodhareist/{Presets, IRs, NAMs} via the
// native file bridge (list/read/write postMessages), plus TONE3000 NAM A2
// search/download (native owns the API key; this page never sees it).
//
// Presets and NAMs are text, so they come back through `readFile`. IRs are
// binary `.wav`: the page sends only the file name (`postLoadIr`) and native
// reads the bytes itself — see `instanceBridge.ts`. TONE3000 loads send only
// a tone id; native fetches the A2 file and loads the NAM engine.

import { useEffect, useRef, useState } from "react";
import { postLoadIr, subscribeIrLoadResult } from "../bridge";
import {
  onNativeMessage,
  postListFiles,
  postReadFile,
  postTone3000LoadTone,
  postTone3000Search,
  postTone3000Status,
  postWriteFile,
  type FileEntry,
  type FileKind,
  type Tone3000ToneCard,
} from "../instanceBridge";
import {
  parsePresetFile,
  seedFactoryPresets,
  type PresetFile,
} from "../presetFiles";
import type { RigSnapshot } from "../Editor";

type PresetBrowserProps = {
  currentPresetId: string;
  modifiedIds?: ReadonlySet<string>;
  /** Apply a successfully parsed preset file to the rig. */
  onLoadPresetFile: (file: PresetFile) => void;
  /** Build the save payload for a user preset name; null cancels. */
  buildSavePayload: (name: string) => { fileName: string; content: string } | null;
  /** Editor's factorySnapshot — used once to seed an empty Presets folder. */
  buildFactorySnapshot: (id: string) => RigSnapshot | null;
  /** Route a `.nam` file's text into the amp slot's NAM engine. */
  onLoadNamFile: (name: string, json: string) => void;
  /** Switch the amp slot to NAM A2 before a TONE3000 load. */
  onPrepareNamEngine?: () => void;
  /** Called after an IR loads, so the editor can switch the cabinet slot to
   * the convolution engine — loading and selecting are separate steps. */
  onIrLoaded?: (name: string) => void;
};

const TABS: { kind: FileKind; label: string }[] = [
  { kind: "presets", label: "Preset" },
  { kind: "irs", label: "IR" },
  { kind: "nams", label: "NAM A2" },
];

type NamSource = "local" | "tone3000";

/** `"01A Twin Sparkle.json"` → `{ pid: "01A", pname: "Twin Sparkle" }`. */
function displayParts(fileName: string): { pid: string; pname: string } {
  const stem = fileName.replace(/\.[^.]+$/, "");
  const space = stem.indexOf(" ");
  if (space > 0 && space <= 4) {
    return { pid: stem.slice(0, space), pname: stem.slice(space + 1) };
  }
  return { pid: "•", pname: stem };
}

export function PresetBrowser({
  currentPresetId,
  modifiedIds,
  onLoadPresetFile,
  buildSavePayload,
  buildFactorySnapshot,
  onLoadNamFile,
  onPrepareNamEngine,
  onIrLoaded,
}: PresetBrowserProps) {
  const [tab, setTab] = useState<FileKind>("presets");
  const [query, setQuery] = useState("");
  const [lists, setLists] = useState<Partial<Record<FileKind, FileEntry[]>>>({});
  const [saving, setSaving] = useState(false);
  const [saveName, setSaveName] = useState("");
  const [status, setStatus] = useState<string | null>(null);
  /** File name of the IR the DSP currently has loaded, if any. */
  const [loadedIr, setLoadedIr] = useState<string | null>(null);
  const [namSource, setNamSource] = useState<NamSource>("local");
  const [tone3000Configured, setTone3000Configured] = useState<boolean | null>(null);
  const [tone3000Error, setTone3000Error] = useState<string | null>(null);
  const [tone3000Tones, setTone3000Tones] = useState<Tone3000ToneCard[]>([]);
  const [tone3000Loading, setTone3000Loading] = useState(false);
  const [loadedToneId, setLoadedToneId] = useState<number | null>(null);
  const seededRef = useRef(false);
  const pendingSeedWrites = useRef(0);
  const searchTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(
    () =>
      onNativeMessage((msg) => {
        if (msg.type === "futureboard.fileList") {
          setLists((prev) => ({ ...prev, [msg.kind]: msg.files }));
          if (
            msg.kind === "presets" &&
            msg.files.length === 0 &&
            !seededRef.current
          ) {
            seededRef.current = true;
            pendingSeedWrites.current = seedFactoryPresets(
              buildFactorySnapshot,
              (fileName, content) => postWriteFile("presets", fileName, content),
            );
            if (pendingSeedWrites.current > 0) {
              setStatus("Creating factory presets…");
            }
          }
        } else if (msg.type === "futureboard.fileWritten") {
          if (pendingSeedWrites.current > 0) {
            pendingSeedWrites.current -= 1;
            if (pendingSeedWrites.current === 0) {
              setStatus(null);
              postListFiles("presets");
            }
          } else if (msg.kind === "presets") {
            setStatus(msg.ok ? null : `Save failed: ${msg.error ?? "unknown"}`);
            postListFiles("presets");
          }
        } else if (msg.type === "futureboard.fileContent") {
          if (!msg.ok || typeof msg.content !== "string") {
            setStatus(`Read failed: ${msg.error ?? "unknown"}`);
            return;
          }
          if (msg.kind === "presets") {
            const parsed = parsePresetFile(msg.content);
            if (parsed) {
              setStatus(null);
              onLoadPresetFile(parsed);
            } else {
              setStatus(`Not a Rodhareist preset: ${msg.fileName}`);
            }
          } else if (msg.kind === "nams") {
            onLoadNamFile(msg.fileName.replace(/\.nam$/i, ""), msg.content);
          }
        } else if (msg.type === "futureboard.tone3000Status") {
          setTone3000Configured(msg.configured);
          setTone3000Error(msg.configured ? null : (msg.error ?? null));
        } else if (msg.type === "futureboard.tone3000SearchResult") {
          setTone3000Loading(false);
          if (!msg.ok) {
            setTone3000Tones([]);
            setTone3000Error(msg.error ?? "TONE3000 search failed");
            return;
          }
          setTone3000Error(null);
          setTone3000Tones(msg.tones);
        } else if (msg.type === "futureboard.tone3000LoadResult") {
          if (!msg.ok) {
            setLoadedToneId(null);
            setStatus(`TONE3000 failed: ${msg.error ?? "unknown"}`);
            return;
          }
          setLoadedToneId(msg.toneId);
          setStatus(
            msg.fileName
              ? `TONE3000: loaded ${msg.name}`
              : `TONE3000: loading ${msg.name}…`,
          );
        } else if (msg.type === "futureboard.namCaptureResult") {
          if (msg.ok) {
            const badge =
              msg.family === "a2"
                ? "NAM A2"
                : msg.family === "lstm"
                  ? "NAM LSTM"
                  : "NAM";
            setStatus(`Loaded ${badge}: ${msg.name}`);
          } else {
            setStatus(`NAM failed: ${msg.error ?? "unknown"}`);
          }
        }
      }),
    [buildFactorySnapshot, onLoadPresetFile, onLoadNamFile],
  );

  useEffect(
    () =>
      subscribeIrLoadResult((result) => {
        if (!result.ok) {
          setLoadedIr(null);
          setStatus(`IR failed: ${result.error ?? "unknown"}`);
          return;
        }
        setLoadedIr(result.name);
        const detail = [
          result.stereo ? "stereo" : "mono",
          `${result.frames} frames`,
          result.truncated ? "truncated" : null,
        ]
          .filter(Boolean)
          .join(" · ");
        setStatus(`IR loaded: ${detail}`);
        onIrLoaded?.(result.name);
      }),
    [onIrLoaded],
  );

  useEffect(() => {
    postListFiles(tab);
    if (tab === "nams") postTone3000Status();
  }, [tab]);

  useEffect(() => {
    if (tab !== "nams" || namSource !== "tone3000") return;
    if (tone3000Configured === false) return;
    if (searchTimer.current) clearTimeout(searchTimer.current);
    searchTimer.current = setTimeout(() => {
      setTone3000Loading(true);
      postTone3000Search(query, 1);
    }, query.trim() ? 220 : 0);
    return () => {
      if (searchTimer.current) clearTimeout(searchTimer.current);
    };
  }, [tab, namSource, query, tone3000Configured]);

  const entries = (lists[tab] ?? []).filter((f) =>
    f.fileName.toLowerCase().includes(query.trim().toLowerCase()),
  );

  const commitSave = () => {
    const name = saveName.trim();
    if (!name) {
      setSaving(false);
      return;
    }
    const payload = buildSavePayload(name);
    if (payload) {
      postWriteFile("presets", payload.fileName, payload.content);
      setStatus("Saving…");
    }
    setSaving(false);
    setSaveName("");
  };

  const showTone3000 = tab === "nams" && namSource === "tone3000";

  return (
    <aside className="browser">
      <div className="browser-tabs" role="tablist" aria-label="Plugin files">
        {TABS.map((t) => (
          <button
            key={t.kind}
            type="button"
            role="tab"
            aria-selected={tab === t.kind}
            className={`browser-tab${tab === t.kind ? " active" : ""}`}
            onClick={() => setTab(t.kind)}
          >
            {t.label}
          </button>
        ))}
      </div>

      {tab === "nams" && (
        <div className="browser-source" role="tablist" aria-label="NAM source">
          <button
            type="button"
            role="tab"
            aria-selected={namSource === "local"}
            className={`browser-source-btn${namSource === "local" ? " active" : ""}`}
            onClick={() => setNamSource("local")}
          >
            Local
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={namSource === "tone3000"}
            className={`browser-source-btn${namSource === "tone3000" ? " active" : ""}`}
            onClick={() => setNamSource("tone3000")}
          >
            TONE3000
          </button>
        </div>
      )}

      <div className="search-wrap">
        <input
          type="text"
          className="search"
          placeholder={showTone3000 ? "Search TONE3000 NAM A2…" : "Search…"}
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          aria-label={showTone3000 ? "Search TONE3000" : `Search ${tab}`}
        />
      </div>

      <div className="preset-list" role="listbox">
        {showTone3000 ? (
          <>
            {tone3000Configured === false && (
              <div className="browser-empty">
                {tone3000Error ??
                  "TONE3000 is not configured for this build."}
              </div>
            )}
            {tone3000Configured !== false &&
              tone3000Loading &&
              tone3000Tones.length === 0 && (
                <div className="browser-empty">Searching TONE3000…</div>
              )}
            {tone3000Configured !== false &&
              !tone3000Loading &&
              tone3000Tones.length === 0 && (
                <div className="browser-empty">
                  {tone3000Error ?? "No NAM A2 tones match."}
                </div>
              )}
            {tone3000Tones.map((tone) => {
              const active = tone.id === loadedToneId;
              return (
                <button
                  key={tone.id}
                  type="button"
                  role="option"
                  aria-selected={active}
                  className={`preset-item${active ? " active" : ""}`}
                  title={`${tone.title} · ${tone.gear} · @${tone.creator}`}
                  onClick={() => {
                    onPrepareNamEngine?.();
                    setStatus(`Fetching ${tone.title}…`);
                    setLoadedToneId(tone.id);
                    postTone3000LoadTone(tone.id, {
                      stereo: true,
                      fullRig:
                        tone.gear === "amp-cab" || tone.gear === "full-rig",
                    });
                  }}
                >
                  <span className="dot" />
                  <span className="pid">A2</span>
                  <span className="pname">
                    {tone.title}
                    {tone.creator ? ` · @${tone.creator}` : ""}
                  </span>
                </button>
              );
            })}
          </>
        ) : (
          <>
            {entries.length === 0 && (
              <div className="browser-empty">
                {tab === "presets" && "No presets yet"}
                {tab === "irs" &&
                  "Drop .wav IRs into Documents/Futureboard Studio/Rodhareist/IRs"}
                {tab === "nams" &&
                  "Drop .nam captures into Documents/Futureboard Studio/Rodhareist/NAMs, or browse TONE3000"}
              </div>
            )}
            {entries.map((f) => {
              const { pid, pname } = displayParts(f.fileName);
              const active =
                (tab === "presets" && pid === currentPresetId) ||
                (tab === "irs" && f.fileName === loadedIr);
              const dirty = tab === "presets" && !!modifiedIds?.has(pid);
              return (
                <button
                  key={f.fileName}
                  type="button"
                  role="option"
                  aria-selected={active}
                  className={`preset-item${active ? " active" : ""}`}
                  title={f.fileName}
                  onClick={() => {
                    if (tab === "presets") postReadFile("presets", f.fileName);
                    else if (tab === "nams") postReadFile("nams", f.fileName);
                    else if (tab === "irs") {
                      setStatus(`Loading ${f.fileName}…`);
                      postLoadIr(f.fileName);
                    }
                  }}
                >
                  <span className="dot" />
                  <span className="pid">{pid}</span>
                  <span className="pname">
                    {pname}
                    {dirty ? " *" : ""}
                  </span>
                </button>
              );
            })}
          </>
        )}
      </div>

      {tab === "presets" && (
        <div className="browser-save">
          {saving ? (
            <input
              type="text"
              className="search"
              autoFocus
              placeholder="Preset name…"
              value={saveName}
              onChange={(e) => setSaveName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") commitSave();
                if (e.key === "Escape") {
                  setSaving(false);
                  setSaveName("");
                }
              }}
              onBlur={() => setSaving(false)}
              aria-label="New preset name"
            />
          ) : (
            <button
              type="button"
              className="browser-save-btn"
              onClick={() => setSaving(true)}
            >
              ＋ Save preset
            </button>
          )}
        </div>
      )}

      {status && <div className="browser-note">{status}</div>}
    </aside>
  );
}
