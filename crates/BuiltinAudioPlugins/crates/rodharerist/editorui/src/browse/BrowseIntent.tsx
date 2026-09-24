import {
  createContext,
  useCallback,
  useContext,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import type { CategoryId } from "../data";

export type BrowseContent = "preset" | "nam" | "ir";

export type BrowseIntent = {
  expectedContent?: BrowseContent;
  targetBlock?: CategoryId;
};

type BrowseIntentApi = {
  intent: BrowseIntent;
  setIntent: (intent: BrowseIntent) => void;
  clearIntent: () => void;
};

const BrowseIntentContext = createContext<BrowseIntentApi>({
  intent: {},
  setIntent: () => {},
  clearIntent: () => {},
});

export function BrowseIntentProvider({ children }: { children: ReactNode }) {
  const [intent, setIntentState] = useState<BrowseIntent>({});
  const setIntent = useCallback((next: BrowseIntent) => {
    setIntentState(next);
  }, []);
  const clearIntent = useCallback(() => {
    setIntentState({});
  }, []);
  const value = useMemo(
    () => ({ intent, setIntent, clearIntent }),
    [clearIntent, intent, setIntent],
  );
  return (
    <BrowseIntentContext.Provider value={value}>
      {children}
    </BrowseIntentContext.Provider>
  );
}

export function useBrowseIntent(): BrowseIntentApi {
  return useContext(BrowseIntentContext);
}
