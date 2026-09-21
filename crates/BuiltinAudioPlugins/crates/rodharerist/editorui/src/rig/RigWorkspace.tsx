import type { NamCaptureLoadOptions } from "../bridge";
import type { CategoryId, Param } from "../data";
import { IoStrip } from "../Components/IoStrip";
import { ModuleEditor } from "../Components/ModuleEditor";
import { SignalChain } from "../Components/SignalChain";

export type RigWorkspaceProps = {
  activeCat: CategoryId;
  activeModelId: string;
  stageModels: Record<CategoryId, string>;
  pathOrder: CategoryId[];
  bypassed: Partial<Record<CategoryId, boolean>>;
  params: Param[];
  inputTrim: number;
  outputTrim: number;
  globalBypass: boolean;
  clipboardCat: CategoryId | null;
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

export function RigWorkspace({
  activeCat,
  activeModelId,
  stageModels,
  pathOrder,
  bypassed,
  params,
  inputTrim,
  outputTrim,
  globalBypass,
  clipboardCat,
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
}: RigWorkspaceProps) {
  return (
    <main className="dashboard">
      <div className="io-rail">
        <IoStrip
          side="in"
          compact
          trim={inputTrim}
          onTrimChange={onGlobalParamChange}
        />
        <IoStrip
          side="out"
          compact
          trim={outputTrim}
          onTrimChange={onGlobalParamChange}
          globalBypass={globalBypass}
          onToggleGlobalBypass={onToggleGlobalBypass}
        />
      </div>
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
  );
}
