import { useBoundInstance } from "../state/boundInstance";

/**
 * The editor's view of the currently bound DSP instance.
 *
 * The CEF `__bridge` transport lives in `instanceBridge.ts` and is shared by
 * every workspace. Components should go through this hook (or `bridge.ts`)
 * rather than calling `fetch("__bridge")` themselves.
 */
export function usePluginInstance() {
  return useBoundInstance();
}
