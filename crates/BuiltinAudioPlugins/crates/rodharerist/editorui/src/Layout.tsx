import { useEffect, useRef } from "react";
import type { CategoryId, Param, Preset } from "./data";
import type { NamCaptureLoadOptions } from "./bridge";
import type { AbSlot } from "./state/history";
import { DiscardDialog } from "./Components/DiscardDialog";
import { Footer } from "./Components/Footer";
import { Header } from "./Components/Header";
import { IoStrip } from "./Components/IoStrip";
import { ModuleEditor } from "./Components/ModuleEditor";
import { PresetBrowser } from "./Components/PresetBrowser";
import { SignalChain } from "./Components/SignalChain";

// Supports weights 100-700
import '@fontsource-variable/ibm-plex-sans/wght.css';

/** Sidebar width bounds + persistence for the drag divider. */
const SIDEBAR_DEFAULT_PX = 176;
const SIDEBAR_MIN_PX = 140;
const SIDEBAR_MAX_PX = 360;
const SIDEBAR_WIDTH_KEY = "rodhareist.sidebarWidth";

function clampSidebar(px: number): number {
  return Math.min(SIDEBAR_MAX_PX, Math.max(SIDEBAR_MIN_PX, Math.round(px)));
}

function storedSidebarWidth(): number {
  try {
    const raw = window.localStorage.getItem(SIDEBAR_WIDTH_KEY);
    const parsed = raw === null ? NaN : Number(raw);
    return Number.isFinite(parsed) ? clampSidebar(parsed) : SIDEBAR_DEFAULT_PX;
  } catch {
    return SIDEBAR_DEFAULT_PX;
  }
}

/**
 * Drag divider on the sidebar's right edge. Width is applied straight to the
 * workspace's CSS variable during the drag (no React re-render per move) and
 * persisted on release. Double-click resets to the default.
 */
function SidebarResizer({
  workspaceRef,
}: {
  workspaceRef: React.RefObject<HTMLDivElement | null>;
}) {
  const applyWidth = (px: number) => {
    workspaceRef.current?.style.setProperty("--sidebar-w", `${clampSidebar(px)}px`);
  };

  return (
    <div
      className="sidebar-resize"
      role="separator"
      aria-orientation="vertical"
      aria-label="Resize sidebar"
      onDoubleClick={() => {
        applyWidth(SIDEBAR_DEFAULT_PX);
        try {
          window.localStorage.setItem(SIDEBAR_WIDTH_KEY, String(SIDEBAR_DEFAULT_PX));
        } catch {
          /* no-op */
        }
      }}
      onPointerDown={(e) => {
        e.preventDefault();
        const startX = e.clientX;
        const startW =
          workspaceRef.current
            ?.style.getPropertyValue("--sidebar-w")
            .match(/^(\d+)px$/)?.[1] ?? String(storedSidebarWidth());
        const base = Number(startW) || SIDEBAR_DEFAULT_PX;
        const target = e.currentTarget;
        target.setPointerCapture(e.pointerId);
        let latest = base;
        const onMove = (ev: PointerEvent) => {
          latest = clampSidebar(base + (ev.clientX - startX));
          applyWidth(latest);
        };
        const onUp = () => {
          target.removeEventListener("pointermove", onMove);
          target.removeEventListener("pointerup", onUp);
          target.removeEventListener("pointercancel", onUp);
          try {
            window.localStorage.setItem(SIDEBAR_WIDTH_KEY, String(latest));
          } catch {
            /* no-op */
          }
        };
        target.addEventListener("pointermove", onMove);
        target.addEventListener("pointerup", onUp);
        target.addEventListener("pointercancel", onUp);
      }}
    />
  );
}

export type DiscardPrompt = {
  presetName: string;
  onSave: () => void;
  onDiscard: () => void;
  onCancel: () => void;
};

export type LayoutProps = {
  currentPresetId: string;
  presetName: string;
  modified: boolean;
  dirtyPresetIds: ReadonlySet<string>;
  activeCat: CategoryId;
  activeModelId: string;
  stageModels: Record<CategoryId, string>;
  pathOrder: CategoryId[];
  bypassed: Partial<Record<CategoryId, boolean>>;
  params: Param[];
  testing: boolean;
  showTestDi: boolean;
  inputTrim: number;
  outputTrim: number;
  globalBypass: boolean;
  canUndo: boolean;
  canRedo: boolean;
  abSlot: AbSlot;
  snapshotSlots: readonly import("./Editor").Snapshot[];
  activeSnapshotIndex: number;
  onSelectSnapshot: (index: number) => void;
  onSaveSnapshot: (index: number) => void;
  onRenameSnapshot: (index: number, name: string) => void;
  clipboardCat: CategoryId | null;
  discardPrompt: DiscardPrompt | null;
  onUndo: () => void;
  onRedo: () => void;
  onSelectAb: (slot: AbSlot) => void;
  onCopyAb: () => void;
  onStepPreset: (dir: number) => void;
  onLoadPresetFile: (file: import("./presetFiles").PresetFile) => void;
  buildSavePayload: (name: string) => { fileName: string; content: string } | null;
  buildFactorySnapshot: (id: string) => import("./Editor").RigSnapshot | null;
  onLoadNamFile: (name: string, json: string) => void;
  onPrepareNamEngine: () => void;
  onIrLoaded: (name: string) => void;
  onToggleTest: () => void;
  onSave: () => void;
  onRevert: () => void;
  onSelectCategory: (cat: CategoryId) => void;
  onToggleModule: (cat: CategoryId) => void;
  onReorderPath: (next: CategoryId[]) => void;
  onSelectModel: (id: string) => void;
  onToggleBypass: () => void;
  onToggleGlobalBypass: () => void;
  onParamChange: (id: string, value: number) => void;
  onGlobalParamChange: (id: string, value: number) => void;
  onCopySettings: (cat: CategoryId) => void;
  onPasteSettings: (cat: CategoryId) => void;
  onResetModule: (cat: CategoryId) => void;
  onLoadNamCapture: (json: string, opts: NamCaptureLoadOptions) => void;
  onBypassCab: () => void;
};

export function Layout({
  currentPresetId,
  presetName,
  modified,
  dirtyPresetIds,
  activeCat,
  activeModelId,
  stageModels,
  pathOrder,
  bypassed,
  params,
  testing,
  showTestDi,
  inputTrim,
  outputTrim,
  globalBypass,
  canUndo,
  canRedo,
  abSlot,
  snapshotSlots,
  activeSnapshotIndex,
  onSelectSnapshot,
  onSaveSnapshot,
  onRenameSnapshot,
  clipboardCat,
  discardPrompt,
  onUndo,
  onRedo,
  onSelectAb,
  onCopyAb,
  onStepPreset,
  onLoadPresetFile,
  buildSavePayload,
  buildFactorySnapshot,
  onLoadNamFile,
  onPrepareNamEngine,
  onIrLoaded,
  onToggleTest,
  onSave,
  onRevert,
  onSelectCategory,
  onToggleModule,
  onReorderPath,
  onSelectModel,
  onToggleBypass,
  onToggleGlobalBypass,
  onParamChange,
  onGlobalParamChange,
  onCopySettings,
  onPasteSettings,
  onResetModule,
  onLoadNamCapture,
  onBypassCab,
}: LayoutProps) {
  const workspaceRef = useRef<HTMLDivElement | null>(null);

  // Restore the persisted sidebar width once per mount.
  useEffect(() => {
    workspaceRef.current?.style.setProperty(
      "--sidebar-w",
      `${storedSidebarWidth()}px`,
    );
  }, []);

  return (
    <div className="plugin">
      <Header
        presetId={currentPresetId}
        presetName={presetName}
        modified={modified}
        testing={testing}
        showTestDi={showTestDi}
        canUndo={canUndo}
        canRedo={canRedo}
        abSlot={abSlot}
        onUndo={onUndo}
        onRedo={onRedo}
        onSelectAb={onSelectAb}
        onCopyAb={onCopyAb}
        onStepPreset={onStepPreset}
        onToggleTest={onToggleTest}
        onSave={onSave}
        onRevert={onRevert}
        snapshotSlots={snapshotSlots}
        activeSnapshotIndex={activeSnapshotIndex}
        onSelectSnapshot={onSelectSnapshot}
        onSaveSnapshot={onSaveSnapshot}
        onRenameSnapshot={onRenameSnapshot}
      />

      <div className="workspace" ref={workspaceRef}>
        <PresetBrowser
          currentPresetId={currentPresetId}
          modifiedIds={dirtyPresetIds}
          onLoadPresetFile={onLoadPresetFile}
          buildSavePayload={buildSavePayload}
          buildFactorySnapshot={buildFactorySnapshot}
          onLoadNamFile={onLoadNamFile}
          onPrepareNamEngine={onPrepareNamEngine}
          onIrLoaded={onIrLoaded}
        />
        <SidebarResizer workspaceRef={workspaceRef} />

        <main className="dashboard">
          {/* Gain staging brackets the chain so input and output levels are
              always on screen, in the order the signal actually travels. */}
          <div className="chain-region">
            <IoStrip side="in" trim={inputTrim} onTrimChange={onGlobalParamChange} />
            <SignalChain
              pathOrder={pathOrder}
              activeCat={activeCat}
              stageModels={stageModels}
              bypassed={bypassed}
              clipboardCat={clipboardCat}
              onSelectCategory={onSelectCategory}
              onToggleModule={onToggleModule}
              onReorderPath={onReorderPath}
              onCopySettings={onCopySettings}
              onPasteSettings={onPasteSettings}
              onResetModule={onResetModule}
            />
            <IoStrip
              side="out"
              trim={outputTrim}
              onTrimChange={onGlobalParamChange}
              globalBypass={globalBypass}
              onToggleGlobalBypass={onToggleGlobalBypass}
            />
          </div>

          <ModuleEditor
            activeCat={activeCat}
            activeModelId={activeModelId}
            bypassed={!!bypassed[activeCat]}
            params={params}
            onSelectModel={onSelectModel}
            onToggleBypass={onToggleBypass}
            onParamChange={onParamChange}
            onLoadNamCapture={onLoadNamCapture}
            onBypassCab={onBypassCab}
          />
        </main>
      </div>

      <Footer globalBypass={globalBypass} />

      {discardPrompt && (
        <DiscardDialog
          presetName={discardPrompt.presetName}
          onSave={discardPrompt.onSave}
          onDiscard={discardPrompt.onDiscard}
          onCancel={discardPrompt.onCancel}
        />
      )}
    </div>
  );
}
