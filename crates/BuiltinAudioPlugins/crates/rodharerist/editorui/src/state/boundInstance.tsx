// Centralized binding to "which DSP instance is this shared page showing
// right now" — the piece the multiplexed built-in editor needs that a
// single-instance page never did. See module doc in `../instanceBridge.ts`
// for the wire protocol this drives.
//
// Native remains authoritative (spec: "the native host decides which
// instance is active and validates all mutations"). This provider never
// activates a route by itself — it either reflects a `selectInstance` native
// already approved, or asks native to approve one (`requestSelectInstance`)
// and waits.

import {
  createContext,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { workspaceSuffix } from "../app/workspace";
import {
  clearActiveParamBinding,
  onNativeMessage,
  requestSelectInstance,
  sendBridgeReady,
  sendInstanceReady,
  setActiveParamBinding,
  type InstanceDisplayMetadata,
} from "../instanceBridge";

export type ConnectionStatus =
  | "disconnected"
  | "waiting"
  | "switching"
  | "active"
  | "error";

export type BoundInstanceState = {
  pluginId: string | null;
  instanceId: string | null;
  bindingGeneration: number;
  display: InstanceDisplayMetadata | null;
  connectionStatus: ConnectionStatus;
  /** The instance's persisted `RodhareistState` JSON (already parsed), `{}`
   * for a fresh insert. Mapped into editor state by `snapshotFromRodhareistState`. */
  state: unknown;
};

const initialState: BoundInstanceState = {
  pluginId: null,
  instanceId: null,
  bindingGeneration: 0,
  display: null,
  connectionStatus: "waiting",
  state: null,
};

const BoundInstanceContext = createContext<BoundInstanceState>(initialState);

export function useBoundInstance(): BoundInstanceState {
  return useContext(BoundInstanceContext);
}

/** Must match `UI_ORIGIN` / the catalog id native routes this editor under. */
const PLUGIN_ID = "rodharerist";

function instanceIdFromPath(pathname: string): string | null {
  const parts = pathname.split("/").filter(Boolean);
  if (parts[0] !== "instance" || !parts[1]) return null;
  return parts[1];
}

export function BoundInstanceProvider({ children }: { children: ReactNode }) {
  const navigate = useNavigate();
  const location = useLocation();
  const [state, setState] = useState<BoundInstanceState>(initialState);

  // The instance id the last *approved* `selectInstance` set. Lets the route
  // effect below tell "native just navigated us here" apart from "the route
  // changed some other way (typed URL, back/forward)" without a render race.
  const approvedInstanceRef = useRef<string | null>(null);
  const pathRef = useRef(location.pathname);
  pathRef.current = location.pathname;

  useEffect(() => {
    sendBridgeReady(PLUGIN_ID);
    const off = onNativeMessage((msg) => {
      if (msg.type === "futureboard.selectInstance") {
        approvedInstanceRef.current = msg.instanceId;
        // Rebind the param write path *before* anything renders against the
        // new instance — also drops any pending coalesced edits made under
        // the previous binding.
        setActiveParamBinding({
          pluginId: msg.pluginId,
          instanceId: msg.instanceId,
          bindingGeneration: msg.bindingGeneration,
        });
        setState({
          pluginId: msg.pluginId,
          instanceId: msg.instanceId,
          bindingGeneration: msg.bindingGeneration,
          display: msg.display,
          connectionStatus: "active",
          state: msg.state,
        });
        navigate(
          `/instance/${msg.instanceId}${workspaceSuffix(pathRef.current)}`,
          { replace: true },
        );
        // Acknowledge only after the state above is committed — React 19
        // batches this synchronously within the handler, so by the time this
        // runs the bound state this instance will render with is already set.
        sendInstanceReady(
          msg.pluginId,
          msg.instanceId,
          msg.bindingGeneration,
          msg.stateRevision,
        );
      } else if (msg.type === "futureboard.instanceRemoved") {
        if (approvedInstanceRef.current === msg.instanceId) {
          approvedInstanceRef.current = null;
          clearActiveParamBinding();
          setState((prev) => ({ ...prev, connectionStatus: "waiting" }));
        }
      }
    });
    return off;
    // Runs once: `navigate` is stable per React Router, and re-sending
    // bridgeReady on every render would re-trigger native's snapshot push.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // The route says an instance native hasn't approved — ask, don't assume.
  // Browse/Rig suffixes are ignored: only the opaque instance token matters.
  useEffect(() => {
    const routeInstanceId = instanceIdFromPath(location.pathname);
    if (!routeInstanceId) return;
    if (routeInstanceId === approvedInstanceRef.current) return;
    requestSelectInstance(routeInstanceId);
  }, [location.pathname]);

  return (
    <BoundInstanceContext.Provider value={state}>
      {children}
    </BoundInstanceContext.Provider>
  );
}
