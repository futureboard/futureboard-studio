import { useEffect, useState } from "react";
import { useWorkspaceNav } from "../app/useWorkspaceNav";
import { useBrowseIntent } from "../browse/BrowseIntent";
import {
  categories,
  defaultValueFor,
  models,
  type CategoryId,
  type Param,
} from "../data";
import type { NamCaptureLoadOptions } from "../bridge";
import { onNativeMessage } from "../instanceBridge";
import { distanceCm, micTypeLabel, positionLabel } from "../globals";
import { GateMonitor } from "./GateMonitor";
import { Knob } from "./Knob";
import { ModelPicker } from "./ModelPicker";

/** Lifecycle of the most recent `.nam` load request. */
type NamLoadStatus =
  | { kind: "idle" }
  | { kind: "loading"; name: string }
  | {
      kind: "loaded";
      name: string;
      receptiveField: number;
      family?: string;
      slimmable?: boolean;
      submodelCount?: number;
    }
  | { kind: "error"; name: string; message: string };

function namLoadedLabel(status: Extract<NamLoadStatus, { kind: "loaded" }>): string {
  const family =
    status.family === "a2" || status.slimmable
      ? status.submodelCount && status.submodelCount > 1
        ? `NAM A2 (${status.submodelCount} quality tiers)`
        : "NAM A2"
      : status.family === "lstm"
        ? "NAM LSTM"
        : "NAM A1";
  return `Loaded “${status.name}” · ${family} · ${status.receptiveField} sample receptive field`;
}

type ModuleEditorProps = {
  activeCat: CategoryId;
  activeModelId: string;
  bypassed: boolean;
  params: Param[];
  onSelectModel: (id: string) => void;
  onToggleBypass: () => void;
  onParamChange: (id: string, value: number) => void;
  onLoadNamCapture: (json: string, opts: NamCaptureLoadOptions) => void;
  onBypassCab: () => void;
};

export function ModuleEditor({
  activeCat,
  activeModelId,
  bypassed,
  params,
  onSelectModel,
  onToggleBypass,
  onParamChange,
  onLoadNamCapture,
  onBypassCab,
}: ModuleEditorProps) {
  const list = models[activeCat] ?? [];
  const model = list.find((m) => m.id === activeModelId) ?? list[0];
  const cat = categories[activeCat];
  const isNamCapture = activeCat === "amp" && activeModelId === "nam_capture";
  const isCabinet = activeCat === "cab";
  const gateThresh =
    activeModelId === "gate"
      ? (params.find((p) => p.id === "gate_thresh") ?? null)
      : null;
  const [namStereo, setNamStereo] = useState(true);
  const [namFullRig, setNamFullRig] = useState(false);
  const [namStatus, setNamStatus] = useState<NamLoadStatus>({ kind: "idle" });
  const [pickerOpen, setPickerOpen] = useState(false);
  const canPickModel = list.length > 1;
  const { go } = useWorkspaceNav();
  const { setIntent } = useBrowseIntent();

  // A category switch (or the active model changing out from under an open
  // picker, e.g. via undo) should not leave a stale picker open over the
  // wrong category's models.
  useEffect(() => {
    setPickerOpen(false);
  }, [activeCat]);

  // Resolve the pending load from the host's async result message.
  useEffect(
    () =>
      onNativeMessage((msg) => {
        if (msg.type !== "futureboard.namCaptureResult") return;
        if (msg.ok) {
          setNamStatus({
            kind: "loaded",
            name: msg.name,
            receptiveField: msg.receptiveField,
            family: msg.family,
            slimmable: msg.slimmable,
            submodelCount: msg.submodelCount,
          });
        } else {
          setNamStatus({
            kind: "error",
            name: msg.name,
            message: msg.error ?? "load failed",
          });
        }
      }),
    [],
  );

  const visibleParams =
    isNamCapture && namStatus.kind === "loaded" && !namStatus.slimmable
      ? params.filter((p) => p.id !== "nam_slim_size")
      : params;
  const paramValue = (id: string, fallback: number) =>
    params.find((p) => p.id === id)?.val ?? fallback;
  const handleCabParamChange = (id: string, value: number) =>
    onParamChange(id, id === "cab_mic_type" ? Math.round(value) : value);

  const handleNamFile = (file: File | undefined) => {
    if (!file) return;
    const reader = new FileReader();
    reader.onload = () => {
      const json = reader.result;
      if (typeof json === "string") {
        const name = file.name.replace(/\.nam$/i, "");
        setNamStatus({ kind: "loading", name });
        onLoadNamCapture(json, {
          name,
          stereo: namStereo,
          fullRig: namFullRig,
        });
      }
    };
    reader.readAsText(file);
  };

  return (
    <section className="editor">
      <div
        className="faceplate"
        style={{ ["--cat-color" as string]: cat.color }}
      >
        <div className="fp-head">
          <div className="fp-identity">
            <span className="fp-stage" style={{ color: cat.color }}>
              {cat.name}
            </span>
            {canPickModel ? (
              <button
                type="button"
                className="fp-name-btn"
                onClick={() => setPickerOpen(true)}
                aria-haspopup="dialog"
                aria-expanded={pickerOpen}
              >
                <span className="fp-name">{model?.name ?? "—"}</span>
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
                  <polyline points="6 9 12 15 18 9" />
                </svg>
              </button>
            ) : (
              <div className="fp-name">{model?.name ?? "—"}</div>
            )}
            <div className="fp-sub">{model?.sub ?? ""}</div>
          </div>
          <div className="fp-actions">
            {isNamCapture && (
              <button
                type="button"
                className="nam-file-btn"
                onClick={() => {
                  setIntent({ expectedContent: "nam", targetBlock: "amp" });
                  go({ mode: "browse", section: "explore" });
                }}
              >
                Replace
              </button>
            )}
            <button
              className={`bypass${bypassed ? " off" : ""}`}
              onClick={onToggleBypass}
              type="button"
              aria-pressed={!bypassed}
            >
              <span className="led" />
              <span>{bypassed ? "Bypassed" : "Active"}</span>
            </button>
          </div>
        </div>

        {isNamCapture && (
          <div className="nam-capture-controls nam-sections">
            <div className="nam-section">
              <div className="nam-section-label">Model</div>
              <div className="nam-model-line">
                <span className="nam-model-name">
                  {namStatus.kind === "loaded"
                    ? namStatus.name
                    : namStatus.kind === "loading"
                      ? namStatus.name
                      : "No capture loaded"}
                </span>
                <span className="nam-model-source">
                  {namStatus.kind === "loaded"
                    ? namStatus.family === "a2" || namStatus.slimmable
                      ? "NAM A2"
                      : namStatus.family === "lstm"
                        ? "NAM LSTM"
                        : "NAM"
                    : namStatus.kind === "loading"
                      ? "Loading…"
                      : "—"}
                </span>
              </div>
              <div className="nam-actions">
                <button
                  type="button"
                  className="nam-file-btn"
                  onClick={() => {
                    setIntent({ expectedContent: "nam", targetBlock: "amp" });
                    go({ mode: "browse", section: "explore" });
                  }}
                >
                  Browse Models
                </button>
                <label className="nam-file-btn">
                  Load Local File
                  <input
                    type="file"
                    accept=".nam"
                    onChange={(e) => handleNamFile(e.target.files?.[0])}
                  />
                </label>
              </div>
              {namStatus.kind !== "idle" && (
                <div
                  className={`nam-load-status ${namStatus.kind}`}
                  role="status"
                  aria-live="polite"
                >
                  {namStatus.kind === "loading" && `Loading “${namStatus.name}”…`}
                  {namStatus.kind === "loaded" && namLoadedLabel(namStatus)}
                  {namStatus.kind === "error" &&
                    `“${namStatus.name}” failed: ${namStatus.message}`}
                </div>
              )}
            </div>
            <div className="nam-section">
              <div className="nam-section-label">Mode</div>
              <label className="nam-check">
                <input
                  type="checkbox"
                  checked={namStereo}
                  onChange={(e) => setNamStereo(e.target.checked)}
                />
                Stereo
              </label>
              <label className="nam-check">
                <input
                  type="checkbox"
                  checked={namFullRig}
                  onChange={(e) => setNamFullRig(e.target.checked)}
                />
                Full Rig Capture
              </label>
              {namFullRig && (
                <button type="button" className="nam-bypass-cab" onClick={onBypassCab}>
                  Bypass Cab
                </button>
              )}
            </div>
          </div>
        )}

        {isCabinet && (
          <div className="nam-actions cab-browse">
            <button
              type="button"
              className="nam-file-btn"
              onClick={() => {
                setIntent({ expectedContent: "ir", targetBlock: "cab" });
                go({ mode: "browse", section: "ir" });
              }}
            >
              Browse IRs
            </button>
          </div>
        )}

        {isCabinet ? (
          // Mic placement is edited with the same knobs as every other module;
          // the readout translates the two parameters into the terms an engineer
          // thinks in (axis position, centimetres).
          <div className="cab-inspector">
            <div className="param-bank">
              {params.map((p) => (
                <Knob
                  key={p.id}
                  id={p.id}
                  name={p.name}
                  min={p.min}
                  max={p.max}
                  value={p.val}
                  unit={p.unit}
                  defaultValue={defaultValueFor(activeModelId, p.id)}
                  onChange={handleCabParamChange}
                />
              ))}
            </div>
            <div className="cab-readout">
              <span>
                <b>{micTypeLabel(paramValue("cab_mic_type", 0))}</b>
              </span>
              <span>
                <b>{positionLabel(paramValue("cab_mic", 20))}</b>{" "}
                {paramValue("cab_mic", 20).toFixed(0)}%
              </span>
              <span>{distanceCm(paramValue("cab_dist", 40)).toFixed(1)} cm</span>
            </div>
            <p className="inspector-note">
              Position is measured from the speaker centre; distance is shown on a
              0–30 cm scale. Capsule type, cone position, proximity, air absorption
              and the first room reflection are modelled independently.
            </p>
          </div>
        ) : (
          <div className="param-bank">
            {isNamCapture && (
              <div className="nam-section-label param-bank-label">Processing</div>
            )}
            {visibleParams.map((p) => (
              <Knob
                key={p.id}
                id={p.id}
                name={p.name}
                min={p.min}
                max={p.max}
                value={p.val}
                unit={p.unit}
                defaultValue={defaultValueFor(activeModelId, p.id)}
                onChange={onParamChange}
              />
            ))}
            {gateThresh && (
              <GateMonitor
                paramId={gateThresh.id}
                threshold={gateThresh.val}
                min={gateThresh.min}
                max={gateThresh.max}
                onChange={onParamChange}
              />
            )}
          </div>
        )}
      </div>

      {pickerOpen && canPickModel && (
        <ModelPicker
          cat={activeCat}
          models={list}
          activeModelId={activeModelId}
          onSelect={onSelectModel}
          onClose={() => setPickerOpen(false)}
        />
      )}
    </section>
  );
}
