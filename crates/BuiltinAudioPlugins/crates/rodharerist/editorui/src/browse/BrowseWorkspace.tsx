// Library browser: Presets / Explore / NAM / IR / Local / Recents.
//
// File and TONE3000 traffic still use the shared `__bridge` transport in
// `instanceBridge.ts`. This workspace never opens its own IPC connection.

import { useEffect, useMemo, useRef, useState } from "react";
import { useWorkspaceNav } from "../app/useWorkspaceNav";
import {
  LIBRARY_LABELS,
  LIBRARY_SECTIONS,
  type LibrarySection,
} from "../app/workspace";
import { postLoadIr, subscribeIrLoadResult } from "../bridge";
import type { RigSnapshot } from "../Editor";
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
import { useBrowseIntent } from "./BrowseIntent";
import { listRecents, rememberRecent, type RecentItem } from "./recents";

export type BrowseWorkspaceProps = {
  currentPresetId: string;
  modifiedIds?: ReadonlySet<string>;
  onLoadPresetFile: (file: PresetFile) => void;
  buildSavePayload: (name: string) => { fileName: string; content: string } | null;
  buildFactorySnapshot: (id: string) => RigSnapshot | null;
  onLoadNamFile: (name: string, json: string) => void;
  onPrepareNamEngine?: () => void;
  onIrLoaded?: (name: string) => void;
};

type Selected =
  | { kind: "file"; fileKind: FileKind; file: FileEntry }
  | { kind: "tone"; tone: Tone3000ToneCard }
  | { kind: "recent"; item: RecentItem };

const SEARCH_DEBOUNCE_MS = 320;

function displayParts(fileName: string): { pid: string; pname: string } {
  const stem = fileName.replace(/\.[^.]+$/, "");
  const space = stem.indexOf(" ");
  if (space > 0 && space <= 4) {
    return { pid: stem.slice(0, space), pname: stem.slice(space + 1) };
  }
  return { pid: "•", pname: stem };
}

function isRateLimited(error: string | null | undefined): boolean {
  if (!error) return false;
  return /429|rate.?limit/i.test(error);
}

function sectionFileKind(section: LibrarySection): FileKind | null {
  if (section === "presets") return "presets";
  if (section === "nam") return "nams";
  if (section === "ir") return "irs";
  return null;
}

export function BrowseWorkspace({
  currentPresetId,
  modifiedIds,
  onLoadPresetFile,
  buildSavePayload,
  buildFactorySnapshot,
  onLoadNamFile,
  onPrepareNamEngine,
  onIrLoaded,
}: BrowseWorkspaceProps) {
  const { workspace, go } = useWorkspaceNav();
  const { intent, clearIntent } = useBrowseIntent();
  const section = workspace.mode === "browse" ? workspace.section : "presets";

  const [query, setQuery] = useState("");
  const [lists, setLists] = useState<Partial<Record<FileKind, FileEntry[]>>>({});
  const [saving, setSaving] = useState(false);
  const [saveName, setSaveName] = useState("");
  const [status, setStatus] = useState<string | null>(null);
  const [loadedIr, setLoadedIr] = useState<string | null>(null);
  const [tone3000Configured, setTone3000Configured] = useState<boolean | null>(
    null,
  );
  const [tone3000Error, setTone3000Error] = useState<string | null>(null);
  const [tone3000Tones, setTone3000Tones] = useState<Tone3000ToneCard[]>([]);
  const [tone3000Page, setTone3000Page] = useState(1);
  const [tone3000Loading, setTone3000Loading] = useState(false);
  const [loadedToneId, setLoadedToneId] = useState<number | null>(null);
  const [selected, setSelected] = useState<Selected | null>(null);
  const [recents, setRecents] = useState<RecentItem[]>(() => listRecents());
  const [online, setOnline] = useState(
    () => typeof navigator === "undefined" || navigator.onLine,
  );
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
              setRecents(
                rememberRecent({
                  kind: "preset",
                  id: parsed.id,
                  title: parsed.name,
                  source: "local",
                  fileName: msg.fileName,
                }),
              );
              onLoadPresetFile(parsed);
              clearIntent();
              go({ mode: "rig" });
            } else {
              setStatus(`Not a Rodhareist preset: ${msg.fileName}`);
            }
          } else if (msg.kind === "nams") {
            setRecents(
              rememberRecent({
                kind: "nam",
                id: msg.fileName,
                title: msg.fileName.replace(/\.nam$/i, ""),
                source: "local",
                fileName: msg.fileName,
              }),
            );
            onLoadNamFile(msg.fileName.replace(/\.nam$/i, ""), msg.content);
            if (intent.targetBlock === "amp" || intent.expectedContent === "nam") {
              clearIntent();
              go({ mode: "rig" });
            }
          }
        } else if (msg.type === "futureboard.tone3000Status") {
          setTone3000Configured(msg.configured);
          setTone3000Error(msg.configured ? null : (msg.error ?? null));
        } else if (msg.type === "futureboard.tone3000SearchResult") {
          setTone3000Loading(false);
          if (!msg.ok) {
            if (msg.page <= 1) setTone3000Tones([]);
            setTone3000Error(msg.error ?? "TONE3000 search failed");
            return;
          }
          setTone3000Error(null);
          setTone3000Tones((prev) =>
            msg.page > 1 ? [...prev, ...msg.tones] : msg.tones,
          );
        } else if (msg.type === "futureboard.tone3000LoadResult") {
          if (!msg.ok) {
            setLoadedToneId(null);
            setStatus(`TONE3000 failed: ${msg.error ?? "unknown"}`);
            return;
          }
          setLoadedToneId(msg.toneId);
          setRecents(
            rememberRecent({
              kind: "tone3000",
              id: String(msg.toneId),
              title: msg.name,
              source: "tone3000",
            }),
          );
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
            if (intent.targetBlock === "amp" || intent.expectedContent === "nam") {
              clearIntent();
              go({ mode: "rig" });
            }
          } else {
            setStatus(`NAM failed: ${msg.error ?? "unknown"}`);
          }
        }
      }),
    [
      buildFactorySnapshot,
      clearIntent,
      go,
      intent.expectedContent,
      intent.targetBlock,
      onLoadNamFile,
      onLoadPresetFile,
    ],
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
        setRecents(
          rememberRecent({
            kind: "ir",
            id: result.name,
            title: result.name,
            source: "local",
            fileName: result.name,
          }),
        );
        const detail = [
          result.stereo ? "stereo" : "mono",
          `${result.frames} frames`,
          result.truncated ? "truncated" : null,
        ]
          .filter(Boolean)
          .join(" · ");
        setStatus(`IR loaded: ${detail}`);
        onIrLoaded?.(result.name);
        if (intent.targetBlock === "cab" || intent.expectedContent === "ir") {
          clearIntent();
          go({ mode: "rig" });
        }
      }),
    [clearIntent, go, intent.expectedContent, intent.targetBlock, onIrLoaded],
  );

  useEffect(() => {
    const on = () => setOnline(true);
    const off = () => setOnline(false);
    window.addEventListener("online", on);
    window.addEventListener("offline", off);
    return () => {
      window.removeEventListener("online", on);
      window.removeEventListener("offline", off);
    };
  }, []);

  useEffect(() => {
    postListFiles("presets");
    postListFiles("irs");
    postListFiles("nams");
    if (section === "explore") postTone3000Status();
  }, [section]);

  useEffect(() => {
    if (section !== "explore") return;
    if (!online) return;
    if (tone3000Configured === false) return;
    if (searchTimer.current) clearTimeout(searchTimer.current);
    searchTimer.current = setTimeout(() => {
      setTone3000Loading(true);
      setTone3000Page(1);
      postTone3000Search(query, 1);
    }, query.trim() ? SEARCH_DEBOUNCE_MS : 0);
    return () => {
      if (searchTimer.current) clearTimeout(searchTimer.current);
    };
  }, [section, query, tone3000Configured, online]);

  const localEntries = useMemo(() => {
    const match = (f: FileEntry) =>
      f.fileName.toLowerCase().includes(query.trim().toLowerCase());
    if (section === "local") {
      return [
        ...(lists.nams ?? []).map((file) => ({ fileKind: "nams" as const, file })),
        ...(lists.irs ?? []).map((file) => ({ fileKind: "irs" as const, file })),
      ].filter((row) => match(row.file));
    }
    const kind = sectionFileKind(section);
    if (!kind) return [];
    return (lists[kind] ?? [])
      .filter(match)
      .map((file) => ({ fileKind: kind, file }));
  }, [lists, query, section]);

  const filteredRecents = useMemo(() => {
    const q = query.trim().toLowerCase();
    return recents.filter(
      (item) =>
        !q ||
        item.title.toLowerCase().includes(q) ||
        item.source.toLowerCase().includes(q),
    );
  }, [query, recents]);

  const showExplore = section === "explore";

  const loadFile = (fileKind: FileKind, fileName: string) => {
    if (fileKind === "presets") postReadFile("presets", fileName);
    else if (fileKind === "nams") {
      onPrepareNamEngine?.();
      postReadFile("nams", fileName);
    } else if (fileKind === "irs") {
      setStatus(`Loading ${fileName}…`);
      postLoadIr(fileName);
    }
  };

  const loadTone = (tone: Tone3000ToneCard) => {
    onPrepareNamEngine?.();
    setStatus(`Fetching ${tone.title}…`);
    setLoadedToneId(tone.id);
    postTone3000LoadTone(tone.id, {
      stereo: true,
      fullRig: tone.gear === "amp-cab" || tone.gear === "full-rig",
    });
  };

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

  const loadMore = () => {
    if (tone3000Loading) return;
    const next = tone3000Page + 1;
    setTone3000Page(next);
    setTone3000Loading(true);
    postTone3000Search(query, next);
  };

  const retryExplore = () => {
    setTone3000Error(null);
    setTone3000Loading(true);
    setTone3000Page(1);
    postTone3000Status();
    postTone3000Search(query, 1);
  };

  const loadRecent = (item: RecentItem) => {
    if (item.kind === "tone3000") {
      const toneId = Number(item.id);
      if (!Number.isFinite(toneId)) return;
      onPrepareNamEngine?.();
      setStatus(`Fetching ${item.title}…`);
      postTone3000LoadTone(toneId, { stereo: true, fullRig: false });
      return;
    }
    if (item.kind === "preset") {
      if (item.fileName) postReadFile("presets", item.fileName);
      else go({ mode: "browse", section: "presets" });
      return;
    }
    if (item.kind === "nam") {
      onPrepareNamEngine?.();
      postReadFile("nams", item.fileName ?? item.id);
      return;
    }
    setStatus(`Loading ${item.title}…`);
    postLoadIr(item.fileName ?? item.id);
  };

  const contextualLabel = (content: "nam" | "ir" | "preset") => {
    if (intent.targetBlock === "amp" && content === "nam") {
      return "Replace NAM A2";
    }
    if (intent.targetBlock === "cab" && content === "ir") {
      return "Replace Cab IR";
    }
    if (content === "preset") return "Load preset";
    if (content === "nam") return "Load NAM";
    return "Load IR";
  };

  return (
    <div className="browse-shell">
      <nav className="library-nav" aria-label="Library">
        {LIBRARY_SECTIONS.map((id) => (
          <button
            key={id}
            type="button"
            className={`library-nav-btn${section === id ? " active" : ""}`}
            onClick={() => go({ mode: "browse", section: id })}
          >
            {LIBRARY_LABELS[id]}
          </button>
        ))}
      </nav>

      <section className="browse-results">
        <div className="browse-results-head">
          <div className="browse-results-title">
            {LIBRARY_LABELS[section]}
            {intent.expectedContent && (
              <span className="browse-intent">
                {intent.expectedContent === "nam"
                  ? "NAM for selected block"
                  : intent.expectedContent === "ir"
                    ? "IR for selected block"
                    : "Presets"}
              </span>
            )}
          </div>
          <input
            type="text"
            className="search"
            placeholder={
              showExplore ? "Search TONE3000 NAM A2…" : `Search ${LIBRARY_LABELS[section].toLowerCase()}…`
            }
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            aria-label={showExplore ? "Search TONE3000" : `Search ${section}`}
          />
        </div>

        <div className="preset-list browse-list" role="listbox">
          {showExplore ? (
            <>
              {!online && (
                <div className="browser-empty">
                  Offline. Local NAM, IR, and presets still work from their library pages.
                </div>
              )}
              {online && tone3000Configured === false && (
                <div className="browser-empty">
                  {tone3000Error ??
                    "TONE3000 is not configured. Local NAM files still work from NAM Models."}
                </div>
              )}
              {online &&
                tone3000Configured !== false &&
                isRateLimited(tone3000Error) && (
                  <div className="browser-empty browse-error">
                    TONE3000 rate-limited this search. Wait a moment, then retry.
                    <button
                      type="button"
                      className="browser-save-btn"
                      onClick={retryExplore}
                    >
                      Retry
                    </button>
                  </div>
                )}
              {online &&
                tone3000Configured !== false &&
                tone3000Loading &&
                tone3000Tones.length === 0 && (
                  <div className="browser-empty">Searching TONE3000…</div>
                )}
              {online &&
                tone3000Configured !== false &&
                !tone3000Loading &&
                tone3000Tones.length === 0 &&
                !isRateLimited(tone3000Error) && (
                  <div className="browser-empty">
                    {tone3000Error ?? "No NAM A2 tones match."}
                    {tone3000Error && (
                      <button
                        type="button"
                        className="browser-save-btn"
                        onClick={retryExplore}
                      >
                        Retry
                      </button>
                    )}
                  </div>
                )}
              {tone3000Tones.map((tone) => {
                const active =
                  selected?.kind === "tone" && selected.tone.id === tone.id;
                return (
                  <button
                    key={tone.id}
                    type="button"
                    role="option"
                    aria-selected={active || tone.id === loadedToneId}
                    className={`preset-item${active || tone.id === loadedToneId ? " active" : ""}`}
                    title={`${tone.title} · ${tone.gear} · @${tone.creator}`}
                    onClick={() => setSelected({ kind: "tone", tone })}
                    onDoubleClick={() => loadTone(tone)}
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
              {online &&
                tone3000Configured !== false &&
                tone3000Tones.length > 0 && (
                <button
                  type="button"
                  className="browser-save-btn"
                  disabled={tone3000Loading}
                  onClick={loadMore}
                >
                  {tone3000Loading ? "Loading…" : "Load more"}
                </button>
              )}
            </>
          ) : section === "recents" ? (
            <>
              {filteredRecents.length === 0 && (
                <div className="browser-empty">
                  Nothing loaded yet this session.
                </div>
              )}
              {filteredRecents.map((item) => {
                const active =
                  selected?.kind === "recent" && selected.item.id === item.id;
                return (
                  <button
                    key={`${item.kind}:${item.id}`}
                    type="button"
                    role="option"
                    aria-selected={active}
                    className={`preset-item${active ? " active" : ""}`}
                    onClick={() => setSelected({ kind: "recent", item })}
                    onDoubleClick={() => loadRecent(item)}
                  >
                    <span className="dot" />
                    <span className="pid">{item.kind === "tone3000" ? "A2" : item.kind}</span>
                    <span className="pname">{item.title}</span>
                  </button>
                );
              })}
            </>
          ) : (
            <>
              {localEntries.length === 0 && (
                <div className="browser-empty">
                  {section === "presets" && "No presets yet"}
                  {section === "ir" &&
                    "Drop .wav IRs into Documents/Futureboard Studio/Rodhareist/IRs"}
                  {section === "nam" &&
                    "Drop .nam captures into Documents/Futureboard Studio/Rodhareist/NAMs, or use Explore"}
                  {section === "local" && "No local NAM or IR files yet"}
                </div>
              )}
              {localEntries.map(({ fileKind, file }) => {
                const { pid, pname } = displayParts(file.fileName);
                const active =
                  (selected?.kind === "file" &&
                    selected.file.fileName === file.fileName) ||
                  (fileKind === "presets" && pid === currentPresetId) ||
                  (fileKind === "irs" && file.fileName === loadedIr);
                const dirty = fileKind === "presets" && !!modifiedIds?.has(pid);
                return (
                  <button
                    key={`${fileKind}:${file.fileName}`}
                    type="button"
                    role="option"
                    aria-selected={active}
                    className={`preset-item${active ? " active" : ""}`}
                    title={file.fileName}
                    onClick={() =>
                      setSelected({ kind: "file", fileKind, file })
                    }
                    onDoubleClick={() => loadFile(fileKind, file.fileName)}
                  >
                    <span className="dot" />
                    <span className="pid">{fileKind === "nams" ? "NAM" : pid}</span>
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

        {section === "presets" && (
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
                Save preset
              </button>
            )}
          </div>
        )}
        {status && <div className="browser-note">{status}</div>}
      </section>

      <aside className="browse-detail" aria-label="Item details">
        {!selected && (
          <div className="browser-empty">
            Select an item to inspect it. Double-click loads immediately.
          </div>
        )}
        {selected?.kind === "tone" && (
          <DetailCard
            title={selected.tone.title}
            rows={[
              ["Source", "TONE3000"],
              ["Author", selected.tone.creator ? `@${selected.tone.creator}` : "—"],
              ["Type", selected.tone.gear || "NAM"],
              ["Format", selected.tone.format || "nam"],
            ]}
            image={selected.tone.image}
            actionLabel={contextualLabel("nam")}
            onAction={() => loadTone(selected.tone)}
          />
        )}
        {selected?.kind === "file" && (
          <DetailCard
            title={displayParts(selected.file.fileName).pname}
            rows={[
              ["Source", "Local"],
              [
                "Kind",
                selected.fileKind === "presets"
                  ? "Preset"
                  : selected.fileKind === "nams"
                    ? "NAM"
                    : "IR",
              ],
              ["File", selected.file.fileName],
            ]}
            actionLabel={contextualLabel(
              selected.fileKind === "presets"
                ? "preset"
                : selected.fileKind === "nams"
                  ? "nam"
                  : "ir",
            )}
            onAction={() => loadFile(selected.fileKind, selected.file.fileName)}
          />
        )}
        {selected?.kind === "recent" && (
          <DetailCard
            title={selected.item.title}
            rows={[
              ["Source", selected.item.source],
              ["Kind", selected.item.kind],
            ]}
            actionLabel={
              selected.item.kind === "preset"
                ? contextualLabel("preset")
                : selected.item.kind === "ir"
                  ? contextualLabel("ir")
                  : contextualLabel("nam")
            }
            onAction={() => loadRecent(selected.item)}
          />
        )}
      </aside>
    </div>
  );
}

function DetailCard({
  title,
  rows,
  image,
  actionLabel,
  onAction,
}: {
  title: string;
  rows: [string, string][];
  image?: string | null;
  actionLabel: string;
  onAction: () => void;
}) {
  return (
    <div className="detail-card">
      {image ? (
        <img className="detail-thumb" src={image} alt="" />
      ) : (
        <div className="detail-thumb empty" aria-hidden />
      )}
      <h2 className="detail-title">{title}</h2>
      <dl className="detail-meta">
        {rows.map(([label, value]) => (
          <div key={label} className="detail-row">
            <dt>{label}</dt>
            <dd>{value}</dd>
          </div>
        ))}
      </dl>
      <button type="button" className="rig-btn primary ready" onClick={onAction}>
        {actionLabel}
      </button>
    </div>
  );
}
