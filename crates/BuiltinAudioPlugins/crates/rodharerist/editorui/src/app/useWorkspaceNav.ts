import { useCallback, useMemo } from "react";
import { useLocation, useNavigate, useParams } from "react-router-dom";
import {
  parseWorkspacePath,
  workspaceHref,
  type Workspace,
} from "./workspace";

/** Hash-route navigation for Browse/Rig. Does not touch the native binding. */
export function useWorkspaceNav() {
  const navigate = useNavigate();
  const location = useLocation();
  const params = useParams<{ instanceId?: string }>();
  const instanceId = params.instanceId ?? null;
  const workspace = useMemo(
    () => parseWorkspacePath(location.pathname),
    [location.pathname],
  );

  const go = useCallback(
    (next: Workspace) => {
      navigate(workspaceHref(instanceId, next));
    },
    [instanceId, navigate],
  );

  return { workspace, go, instanceId };
}
