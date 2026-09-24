import logoUrl from "../Assets/logo.svg";
import { WorkspaceSwitcher } from "../shell/WorkspaceSwitcher";
import type { AbSlot } from "../state/history";
import { SnapshotDropdown } from "./SnapshotDropdown";

type HeaderProps = {
  presetId: string;
  presetName: string;
  modified: boolean;
  testing: boolean;
  showTestDi: boolean;
  canUndo: boolean;
  canRedo: boolean;
  abSlot: AbSlot;
  onUndo: () => void;
  onRedo: () => void;
  onSelectAb: (slot: AbSlot) => void;
  onCopyAb: () => void;
  onStepPreset: (dir: number) => void;
  onToggleTest: () => void;
  onSave: () => void;
  onRevert: () => void;
  snapshotSlots: readonly import("../Editor").Snapshot[];
  activeSnapshotIndex: number;
  onSelectSnapshot: (index: number) => void;
  onSaveSnapshot: (index: number) => void;
  onRenameSnapshot: (index: number, name: string) => void;
};

export function Header({
  presetId,
  presetName,
  modified,
  testing,
  showTestDi,
  canUndo,
  canRedo,
  abSlot,
  onUndo,
  onRedo,
  onSelectAb,
  onCopyAb,
  onStepPreset,
  onToggleTest,
  onSave,
  onRevert,
  snapshotSlots,
  activeSnapshotIndex,
  onSelectSnapshot,
  onSaveSnapshot,
  onRenameSnapshot,
}: HeaderProps) {
  const otherSlot: AbSlot = abSlot === "A" ? "B" : "A";
  return (
    <header className="header">
      <div className="header-left">
        <div className="brand">
          <img src={logoUrl} alt="Rodhareist" />
        </div>
        <WorkspaceSwitcher />
      </div>

      <div className="header-center">
        <div className="preset-nav" aria-label="Rig selector">
          <button
            className="nav-arrow"
            onClick={() => onStepPreset(-1)}
            aria-label="Previous preset"
            type="button"
          >
            <svg
              width="14"
              height="14"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2.4"
            >
              <polyline points="15 18 9 12 15 6" />
            </svg>
          </button>

          <div className="rig-lcd">
            <div className="rig-lcd-top">
              <span className="rig-badge">Rig</span>
              <span className="rig-slot">{presetId}</span>
              {modified && <span className="mod-dot" title="Modified" />}
            </div>
            <div className="rig-name">
              {presetName}
              {modified ? " *" : ""}
            </div>
          </div>

          <button
            className="nav-arrow"
            onClick={() => onStepPreset(1)}
            aria-label="Next preset"
            type="button"
          >
            <svg
              width="14"
              height="14"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2.4"
            >
              <polyline points="9 18 15 12 9 6" />
            </svg>
          </button>
        </div>
      </div>

      <div className="header-right">
        <div className="edit-actions" aria-label="Edit history">
          <button
            type="button"
            className="hdr-btn icon"
            disabled={!canUndo}
            onClick={onUndo}
            title="Undo (Ctrl+Z)"
            aria-label="Undo"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <path d="M3 7v6h6" />
              <path d="M3 13a9 9 0 1 0 3-7.7L3 8" />
            </svg>
          </button>
          <button
            type="button"
            className="hdr-btn icon"
            disabled={!canRedo}
            onClick={onRedo}
            title="Redo (Ctrl+Shift+Z)"
            aria-label="Redo"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <path d="M21 7v6h-6" />
              <path d="M21 13a9 9 0 1 1-3-7.7L21 8" />
            </svg>
          </button>
        </div>

        <div className="ab-group" role="group" aria-label="A/B compare">
          {(["A", "B"] as const).map((slot) => (
            <button
              key={slot}
              type="button"
              className={`ab-btn${abSlot === slot ? " active" : ""}`}
              aria-pressed={abSlot === slot}
              onClick={() => onSelectAb(slot)}
              title={`Compare slot ${slot} — holds a complete rig state`}
            >
              {slot}
            </button>
          ))}
          <button
            type="button"
            className="hdr-btn"
            onClick={onCopyAb}
            title={`Copy slot ${abSlot} over slot ${otherSlot}`}
          >
            {abSlot}→{otherSlot}
          </button>
        </div>

        <SnapshotDropdown
          slots={snapshotSlots}
          active={activeSnapshotIndex}
          onSelect={onSelectSnapshot}
          onSaveCurrent={onSaveSnapshot}
          onRename={onRenameSnapshot}
        />

        <div className="rig-actions">
          <button
            type="button"
            className="rig-btn"
            disabled={!modified}
            onClick={onRevert}
            title="Revert to last saved rig"
          >
            Revert
          </button>
          <button
            type="button"
            className={`rig-btn primary${modified ? " ready" : ""}`}
            disabled={!modified}
            onClick={onSave}
            title="Save current rig edits"
          >
            Save
          </button>
        </div>
        {showTestDi && (
          <div className="di-tester">
            <span className="di-label">Test DI</span>
            <button
              className={`di-btn${testing ? " on" : ""}`}
              onClick={onToggleTest}
              type="button"
              title="Preview tone + meters (browser only)"
            >
              <span className="led" />
              {testing ? "Stop" : "Play"}
            </button>
          </div>
        )}
      </div>
    </header>
  );
}
