import { useWorkspaceNav } from "../app/useWorkspaceNav";

export function WorkspaceSwitcher() {
  const { workspace, go } = useWorkspaceNav();
  return (
    <div className="mode-switch" role="tablist" aria-label="Workspace">
      <button
        type="button"
        role="tab"
        aria-selected={workspace.mode === "browse"}
        className={`mode-switch-btn${workspace.mode === "browse" ? " active" : ""}`}
        onClick={() =>
          go({
            mode: "browse",
            section: workspace.mode === "browse" ? workspace.section : "presets",
          })
        }
      >
        Browse
      </button>
      <button
        type="button"
        role="tab"
        aria-selected={workspace.mode === "rig"}
        className={`mode-switch-btn${workspace.mode === "rig" ? " active" : ""}`}
        onClick={() => go({ mode: "rig" })}
      >
        Rig
      </button>
    </div>
  );
}
