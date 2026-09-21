/** Browse/Rig workspace routing. The instance id is an opaque token. */

export const LIBRARY_SECTIONS = [
  "presets",
  "explore",
  "nam",
  "ir",
  "local",
  "recents",
] as const;

export type LibrarySection = (typeof LIBRARY_SECTIONS)[number];

export type Workspace =
  | { mode: "rig" }
  | { mode: "browse"; section: LibrarySection };

export const LIBRARY_LABELS: Record<LibrarySection, string> = {
  presets: "Presets",
  explore: "Explore",
  nam: "NAM Models",
  ir: "IRs",
  local: "Local Files",
  recents: "Recents",
};

export function isLibrarySection(value: string | undefined): value is LibrarySection {
  return LIBRARY_SECTIONS.includes(value as LibrarySection);
}

/**
 * Read the workspace from a React Router pathname.
 *
 * Accepts both hosted paths (`/instance/<opaqueId>/browse/explore`) and the
 * standalone preview (`/browse/explore`). The instance id is never split on
 * `::` — native owns that string.
 */
export function parseWorkspacePath(pathname: string): Workspace {
  const parts = pathname.split("/").filter(Boolean);
  const rest = parts[0] === "instance" ? parts.slice(2) : parts;
  if (rest[0] === "browse") {
    return {
      mode: "browse",
      section: isLibrarySection(rest[1]) ? rest[1] : "presets",
    };
  }
  return { mode: "rig" };
}

/** Keep the current Browse/Rig suffix when native rebinds an instance. */
export function workspaceSuffix(pathname: string): string {
  const parsed = parseWorkspacePath(pathname);
  return parsed.mode === "browse" ? `/browse/${parsed.section}` : "/rig";
}

export function workspaceHref(
  instanceId: string | null | undefined,
  workspace: Workspace,
): string {
  const leaf =
    workspace.mode === "browse" ? `/browse/${workspace.section}` : "/rig";
  return instanceId ? `/instance/${instanceId}${leaf}` : leaf;
}
