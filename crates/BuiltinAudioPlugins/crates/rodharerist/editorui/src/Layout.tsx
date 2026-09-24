import type { CategoryId, Param } from "./data";
import type { NamCaptureLoadOptions } from "./bridge";
import type { AbSlot } from "./state/history";
import { useWorkspaceNav } from "./app/useWorkspaceNav";
import { BrowseIntentProvider } from "./browse/BrowseIntent";
import { BrowseWorkspace } from "./browse/BrowseWorkspace";
import { DiscardDialog } from "./Components/DiscardDialog";
import { Footer } from "./Components/Footer";
import { Header } from "./Components/Header";
import { RigWorkspace } from "./rig/RigWorkspace";

// Supports weights 100-700
import "@fontsource-variable/ibm-plex-sans/wght.css";

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

export function Layout(props: LayoutProps) {
  const { workspace } = useWorkspaceNav();
  const browse = workspace.mode === "browse";

  return (
    <BrowseIntentProvider>
      <div className="plugin">
        <Header
          presetId={props.currentPresetId}
          presetName={props.presetName}
          modified={props.modified}
          testing={props.testing}
          showTestDi={props.showTestDi}
          canUndo={props.canUndo}
          canRedo={props.canRedo}
          abSlot={props.abSlot}
          onUndo={props.onUndo}
          onRedo={props.onRedo}
          onSelectAb={props.onSelectAb}
          onCopyAb={props.onCopyAb}
          onStepPreset={props.onStepPreset}
          onToggleTest={props.onToggleTest}
          onSave={props.onSave}
          onRevert={props.onRevert}
          snapshotSlots={props.snapshotSlots}
          activeSnapshotIndex={props.activeSnapshotIndex}
          onSelectSnapshot={props.onSelectSnapshot}
          onSaveSnapshot={props.onSaveSnapshot}
          onRenameSnapshot={props.onRenameSnapshot}
        />

        <div className={`workspace ${browse ? "browse-mode" : "rig-mode"}`}>
          {browse && (
            <BrowseWorkspace
              currentPresetId={props.currentPresetId}
              modifiedIds={props.dirtyPresetIds}
              onLoadPresetFile={props.onLoadPresetFile}
              buildSavePayload={props.buildSavePayload}
              buildFactorySnapshot={props.buildFactorySnapshot}
              onLoadNamFile={props.onLoadNamFile}
              onPrepareNamEngine={props.onPrepareNamEngine}
              onIrLoaded={props.onIrLoaded}
            />
          )}
          <div className="workspace-pane" hidden={browse}>
            <RigWorkspace
              activeCat={props.activeCat}
              activeModelId={props.activeModelId}
              stageModels={props.stageModels}
              pathOrder={props.pathOrder}
              bypassed={props.bypassed}
              params={props.params}
              inputTrim={props.inputTrim}
              outputTrim={props.outputTrim}
              globalBypass={props.globalBypass}
              clipboardCat={props.clipboardCat}
              onSelectCategory={props.onSelectCategory}
              onToggleModule={props.onToggleModule}
              onReorderPath={props.onReorderPath}
              onSelectModel={props.onSelectModel}
              onToggleBypass={props.onToggleBypass}
              onToggleGlobalBypass={props.onToggleGlobalBypass}
              onParamChange={props.onParamChange}
              onGlobalParamChange={props.onGlobalParamChange}
              onCopySettings={props.onCopySettings}
              onPasteSettings={props.onPasteSettings}
              onResetModule={props.onResetModule}
              onLoadNamCapture={props.onLoadNamCapture}
              onBypassCab={props.onBypassCab}
            />
          </div>
        </div>

        <Footer globalBypass={props.globalBypass} />

        {props.discardPrompt && (
          <DiscardDialog
            presetName={props.discardPrompt.presetName}
            onSave={props.discardPrompt.onSave}
            onDiscard={props.discardPrompt.onDiscard}
            onCancel={props.discardPrompt.onCancel}
          />
        )}
      </div>
    </BrowseIntentProvider>
  );
}
