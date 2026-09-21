import {
  StrictMode,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { createRoot } from "react-dom/client";
import {
  categories,
  chainOrder,
  completeParameters,
  defaultPath,
  models,
  parameterDefaults,
  parametersForPreset,
  presetsData,
  rackFromPath,
  stageModelsForPreset,
  type CategoryId,
  type Param,
} from "./data";
import { Layout } from "./Layout";
import {
  flushParamEditsNow,
  hasNativeBridge,
  postEnabled,
  postLoadNamCapture,
  postModel,
  postParam,
  postPathOrder,
  type NamCaptureLoadOptions,
} from "./bridge";
import { postGlobalCommand } from "./instanceBridge";
import { POWER_PARAM_ID } from "./globals";
import {
  activeBankSlot,
  activeSnapshot,
  canRedo as historyCanRedo,
  canUndo as historyCanUndo,
  commit,
  copyToOther,
  createAb,
  createHistory,
  redo as historyRedo,
  reset as historyReset,
  saveBankSlot,
  setActive,
  setActiveBankSlot,
  undo as historyUndo,
  type AbSlot,
  type AbState,
  type BankState,
  type History,
} from "./state/history";
import {
  attachHostTelemetry,
  pushSimulatedFrame,
  releasePreview,
} from "./state/meters";
import { snapshotFromRodhareistState } from "./stateMap";
import {
  presetFileName,
  serializePreset,
  type PresetFile,
  type SerializedSnapshotBank,
} from "./presetFiles";
import "./Styles/Editor.css";
import { HashRouter, Routes, Route } from "react-router-dom";
import {
  BoundInstanceProvider,
  useBoundInstance,
} from "./state/boundInstance";

/** Global gain-staging state. Mirrors the DSP's global params exactly. */
export type GlobalState = {
  inputTrim: number;
  outputTrim: number;
  globalBypass: boolean;
};

const DEFAULT_GLOBALS: GlobalState = {
  inputTrim: 0,
  outputTrim: 0,
  globalBypass: false,
};

export type RigSnapshot = {
  activeCat: CategoryId;
  activeModelId: string;
  stageModels: Record<CategoryId, string>;
  pathOrder: CategoryId[];
  bypassed: Partial<Record<CategoryId, boolean>>;
  parameters: Record<string, Param[]>;
  globals: GlobalState;
};

/**
 * One Helix-style performance snapshot: bypass + parameter values only.
 * Deliberately narrower than {@link RigSnapshot} — no `pathOrder` or
 * `stageModels` — so recalling a snapshot can never change which model
 * occupies a stage or reorder the chain, which is what keeps switching
 * click-free (see `mergeSnapshotIntoRig`).
 */
export type Snapshot = {
  name: string;
  bypassed: Partial<Record<CategoryId, boolean>>;
  parameters: Record<string, Param[]>;
};

/** Fixed bank size — matches the number of slots `SnapshotBar` renders. */
export const SNAPSHOT_COUNT = 8;

/**
 * Undo coalescing window. A fader drag emits an edit per pointer move; without
 * this, one gesture would become hundreds of undo steps.
 */
const COMMIT_DEBOUNCE_MS = 300;

function applyCategoryTheme(cat: CategoryId) {
  const c = categories[cat];
  document.documentElement.style.setProperty("--cat-color", c.color);
  document.documentElement.style.setProperty("--cat-rgb", c.rgb);
}

function cloneParameters(
  src: Record<string, Param[]>,
): Record<string, Param[]> {
  const out: Record<string, Param[]> = {};
  for (const [id, params] of Object.entries(src)) {
    out[id] = params.map((p) => ({ ...p }));
  }
  return out;
}

function defaultStageModels(focus: CategoryId, modelId: string) {
  const stageModels = {} as Record<CategoryId, string>;
  for (const cat of chainOrder) {
    stageModels[cat] = models[cat][0]?.id ?? "";
  }
  stageModels[focus] = modelId;
  return stageModels;
}

function makeSnapshot(
  activeCat: CategoryId,
  activeModelId: string,
  stageModels: Record<CategoryId, string>,
  pathOrder: CategoryId[],
  bypassed: Partial<Record<CategoryId, boolean>>,
  parameters: Record<string, Param[]>,
  globals: GlobalState,
): RigSnapshot {
  return {
    activeCat,
    activeModelId,
    stageModels: { ...stageModels },
    pathOrder: [...pathOrder],
    bypassed: { ...bypassed },
    parameters: cloneParameters(parameters),
    globals: { ...globals },
  };
}

/**
 * Structural comparison used to collapse no-op undo commits. Snapshots are
 * small (a few dozen numbers) and this runs at most once per debounce window,
 * so a serialize-and-compare is cheap enough and avoids a hand-written deep
 * equality that would silently miss a newly added field.
 */
function snapshotsEqual(a: RigSnapshot, b: RigSnapshot): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

function applySnapshotToDsp(snap: RigSnapshot) {
  postParam("input_trim", snap.globals.inputTrim);
  postParam("output_trim", snap.globals.outputTrim);
  postParam(POWER_PARAM_ID, snap.globals.globalBypass ? 0 : 1);
  postPathOrder(snap.pathOrder);
  for (const cat of chainOrder) {
    const modelId = snap.stageModels[cat];
    if (modelId) postModel(categories[cat].node, modelId);
    postEnabled(categories[cat].node, !snap.bypassed[cat]);
  }
  for (const cat of chainOrder) {
    const modelId = snap.stageModels[cat];
    for (const param of snap.parameters[modelId] ?? []) {
      postParam(param.id, param.val);
    }
  }
  // A snapshot is one logical transaction. Do not leave it waiting on a CEF
  // animation frame (which may be throttled while the window is rebinding).
  flushParamEditsNow();
}

function snapshotContentFromRig(rig: RigSnapshot, name: string): Snapshot {
  return {
    name,
    bypassed: { ...rig.bypassed },
    parameters: cloneParameters(rig.parameters),
  };
}

/** A fresh bank with all 8 slots seeded identically from `rig` — the state
 * every new preset/instance starts from before anything is explicitly saved
 * into a slot. */
function defaultSnapshotBank(rig: RigSnapshot): BankState<Snapshot> {
  return {
    active: 0,
    slots: Array.from({ length: SNAPSHOT_COUNT }, (_, i) =>
      snapshotContentFromRig(rig, String(i + 1)),
    ),
  };
}

/**
 * Resolve a persisted snapshot bank from a preset file against the current
 * schema, defensively: missing/short/malformed entries fall back to a fresh
 * slot seeded from `fallbackRig`, exactly as `completeParameters` already
 * does for a `RigSnapshot`'s own parameters.
 */
function normalizeSnapshotBank(
  persisted: SerializedSnapshotBank | undefined,
  fallbackRig: RigSnapshot,
): BankState<Snapshot> {
  if (!persisted) return defaultSnapshotBank(fallbackRig);
  const slots = Array.from({ length: SNAPSHOT_COUNT }, (_, i) => {
    const saved = persisted.slots?.[i];
    const fallback = snapshotContentFromRig(fallbackRig, String(i + 1));
    if (!saved || typeof saved !== "object") return fallback;
    return {
      name: typeof saved.name === "string" && saved.name ? saved.name : fallback.name,
      bypassed:
        saved.bypassed && typeof saved.bypassed === "object"
          ? { ...saved.bypassed }
          : fallback.bypassed,
      parameters:
        saved.parameters && typeof saved.parameters === "object"
          ? completeParameters(saved.parameters)
          : fallback.parameters,
    };
  });
  const active =
    typeof persisted.active === "number" &&
    persisted.active >= 0 &&
    persisted.active < SNAPSHOT_COUNT
      ? Math.floor(persisted.active)
      : 0;
  return { active, slots };
}

/**
 * Overlay a snapshot's bypass/parameters onto an otherwise-unchanged rig.
 * Model choice, chain order, active category and global trims all pass
 * through from `rig` untouched — a snapshot recall is never a topology
 * change, which is why it can go through the same DSP push as any other
 * param edit and stay click-free.
 */
function mergeSnapshotIntoRig(rig: RigSnapshot, content: Snapshot): RigSnapshot {
  return {
    ...rig,
    bypassed: { ...content.bypassed },
    parameters: completeParameters(content.parameters),
  };
}

function factorySnapshot(id: string): RigSnapshot | null {
  const p = presetsData.find((x) => x.id === id);
  if (!p) return null;
  const bypassed: Partial<Record<CategoryId, boolean>> = {};
  for (const cat of p.bypassed ?? []) bypassed[cat] = true;
  return {
    activeCat: p.category,
    activeModelId: p.model,
    stageModels: stageModelsForPreset(p),
    pathOrder: p.path ? [...p.path] : defaultPath(),
    bypassed,
    parameters: parametersForPreset(p),
    // The bank is level-matched through Output Trim so each preset can keep
    // the amp settings its tone actually calls for (see `Preset.outputTrim`).
    globals: { ...DEFAULT_GLOBALS, outputTrim: p.outputTrim ?? DEFAULT_GLOBALS.outputTrim },
  };
}

export function RodhareistEditor({
  boundSnapshot = null,
}: {
  /** The bound instance's persisted state, mapped by
   * `snapshotFromRodhareistState`. `null` = fresh insert → factory initial
   * (which is then pushed to the DSP once, becoming the persisted baseline). */
  boundSnapshot?: RigSnapshot | null;
}) {
  const initial = presetsData[1]!;
  const [currentPresetId, setCurrentPresetId] = useState(initial.id);
  const [activeCat, setActiveCat] = useState<CategoryId>(
    boundSnapshot?.activeCat ?? initial.category,
  );
  const [activeModelId, setActiveModelId] = useState(
    boundSnapshot?.activeModelId ?? initial.model,
  );
  const [stageModels, setStageModels] = useState<Record<CategoryId, string>>(
    () =>
      boundSnapshot
        ? { ...boundSnapshot.stageModels }
        : stageModelsForPreset(initial),
  );
  const [pathOrder, setPathOrder] = useState<CategoryId[]>(() =>
    boundSnapshot
      ? [...boundSnapshot.pathOrder]
      : [...(initial.path ?? defaultPath())],
  );
  const [bypassed, setBypassed] = useState<Partial<Record<CategoryId, boolean>>>(
    () => {
      if (boundSnapshot) return { ...boundSnapshot.bypassed };
      const initialBypassed: Partial<Record<CategoryId, boolean>> = {};
      for (const cat of initial.bypassed ?? []) initialBypassed[cat] = true;
      return initialBypassed;
    },
  );
  const [parameters, setParameters] = useState<Record<string, Param[]>>(() =>
    boundSnapshot
      ? completeParameters(boundSnapshot.parameters)
      : parametersForPreset(initial),
  );
  const [globals, setGlobals] = useState<GlobalState>(
    () => boundSnapshot?.globals ?? DEFAULT_GLOBALS,
  );
  const [modified, setModified] = useState(false);
  const [savedRigs, setSavedRigs] = useState<Record<string, RigSnapshot>>({});
  const [drafts, setDrafts] = useState<Record<string, RigSnapshot>>({});
  const [dirtyPresetIds, setDirtyPresetIds] = useState<Set<string>>(
    () => new Set(),
  );
  const [testing, setTesting] = useState(false);
  const [showTestDi] = useState(() => !hasNativeBridge());
  const [pendingSwitchId, setPendingSwitchId] = useState<string | null>(null);

  /** Per-category settings clipboard (Copy/Paste Settings in the block menu). */
  const [clipboard, setClipboard] = useState<{
    cat: CategoryId;
    modelId: string;
    params: Param[];
  } | null>(null);

  const initialSnapshot = useMemo(
    () =>
      boundSnapshot ??
      factorySnapshot(initial.id) ??
      makeSnapshot(
        initial.category,
        initial.model,
        stageModelsForPreset(initial),
        initial.path ?? defaultPath(),
        {},
        parametersForPreset(initial),
        DEFAULT_GLOBALS,
      ),
    // Both inputs are fixed for this mount (remount on instance switch).
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  const [history, setHistory] = useState<History<RigSnapshot>>(() =>
    createHistory(initialSnapshot),
  );
  const [ab, setAb] = useState<AbState<RigSnapshot>>(() =>
    createAb(initialSnapshot),
  );
  const [snapshots, setSnapshots] = useState<BankState<Snapshot>>(() =>
    defaultSnapshotBank(initialSnapshot),
  );

  const audioRef = useRef<{
    ctx: AudioContext;
    gain: GainNode;
    timer: number | null;
  } | null>(null);

  // Keep a ref mirror so callbacks can read current state without stale closures.
  const liveRef = useRef({
    currentPresetId,
    activeCat,
    activeModelId,
    stageModels,
    pathOrder,
    bypassed,
    parameters,
    globals,
    modified,
    drafts,
    savedRigs,
    pendingSwitchId,
    history,
    ab,
    snapshots,
  });
  liveRef.current = {
    currentPresetId,
    activeCat,
    activeModelId,
    stageModels,
    pathOrder,
    bypassed,
    parameters,
    globals,
    modified,
    drafts,
    savedRigs,
    pendingSwitchId,
    history,
    ab,
    snapshots,
  };

  /** Snapshot of the live editor state. */
  const currentSnapshot = useCallback((): RigSnapshot => {
    const l = liveRef.current;
    return makeSnapshot(
      l.activeCat,
      l.activeModelId,
      l.stageModels,
      l.pathOrder,
      l.bypassed,
      l.parameters,
      l.globals,
    );
  }, []);

  const currentPreset = useMemo(
    () =>
      presetsData.find((p) => p.id === currentPresetId) ?? presetsData[0]!,
    [currentPresetId],
  );

  const params =
    parameters[activeModelId] ?? parameterDefaults[activeModelId] ?? [];

  useEffect(() => {
    applyCategoryTheme(activeCat);
  }, [activeCat]);

  // Meter/status telemetry lives outside React: attach the store to the host
  // once. Meter frames never re-render this component.
  useEffect(() => attachHostTelemetry(), []);

  useEffect(() => {
    // Bound to an instance with real persisted state: the DSP already holds
    // it — adopt locally, post NOTHING (a push here used to stomp the newly
    // selected insert with factory defaults on every sidebar switch).
    if (boundSnapshot) return;
    // Fresh insert (no persisted state): establish the factory initial on
    // the DSP once. These posts flow through the normal edit path, so the
    // native state mirror records them as the insert's baseline.
    applySnapshotToDsp(initialSnapshot);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // -------------------------------------------------------------------------
  // Undo/redo plumbing
  // -------------------------------------------------------------------------

  const commitTimerRef = useRef<number | null>(null);
  /** Set while an undo/redo/preset load is applying, to avoid re-recording it. */
  const suppressCommitRef = useRef(false);

  /**
   * Fold the live state into history immediately and return the resulting
   * history.
   *
   * Returns synchronously (rather than only scheduling a `setHistory`) so
   * callers like undo can act on the result within the same tick. `liveRef` is
   * updated in step, because it is only refreshed on render.
   */
  const flushCommit = useCallback((): History<RigSnapshot> => {
    if (commitTimerRef.current !== null) {
      window.clearTimeout(commitTimerRef.current);
      commitTimerRef.current = null;
    }
    const base = liveRef.current.history;
    if (suppressCommitRef.current) return base;
    const next = commit(base, currentSnapshot(), snapshotsEqual);
    if (next !== base) {
      liveRef.current.history = next;
      setHistory(next);
    }
    return next;
  }, [currentSnapshot]);

  /**
   * Record an undo step once the current gesture settles. Called from every
   * mutating action; consecutive calls within the window collapse into one, so
   * a fader drag is a single undo step rather than one per pointer move.
   */
  const scheduleCommit = useCallback(() => {
    if (suppressCommitRef.current) return;
    if (commitTimerRef.current !== null) {
      window.clearTimeout(commitTimerRef.current);
    }
    commitTimerRef.current = window.setTimeout(() => {
      commitTimerRef.current = null;
      flushCommit();
    }, COMMIT_DEBOUNCE_MS);
  }, [flushCommit]);

  useEffect(() => {
    return () => {
      if (commitTimerRef.current !== null) {
        window.clearTimeout(commitTimerRef.current);
      }
    };
  }, []);

  const markDirty = useCallback(() => {
    setModified(true);
    setDirtyPresetIds((prev) => {
      const id = liveRef.current.currentPresetId;
      if (prev.has(id)) return prev;
      const next = new Set(prev);
      next.add(id);
      return next;
    });
    scheduleCommit();
  }, [scheduleCommit]);

  /** Push a snapshot into the live UI state and the DSP. */
  const applyLocalSnapshot = useCallback(
    (snap: RigSnapshot, presetId: string, isDirty: boolean) => {
      setCurrentPresetId(presetId);
      setActiveCat(snap.activeCat);
      setActiveModelId(snap.activeModelId);
      setStageModels(snap.stageModels);
      setPathOrder(snap.pathOrder);
      setBypassed(snap.bypassed);
      setParameters(completeParameters(snap.parameters));
      setGlobals({ ...snap.globals });
      setModified(isDirty);
      applySnapshotToDsp(snap);
    },
    [],
  );

  /**
   * Restore a snapshot as the result of undo/redo or an A/B switch. Suppresses
   * commit recording for the duration so the restore is not itself an edit.
   */
  const restoreSnapshot = useCallback(
    (snap: RigSnapshot, isDirty: boolean) => {
      if (commitTimerRef.current !== null) {
        window.clearTimeout(commitTimerRef.current);
        commitTimerRef.current = null;
      }
      suppressCommitRef.current = true;
      applyLocalSnapshot(snap, liveRef.current.currentPresetId, isDirty);
      // Release after the resulting render has flushed its effects.
      window.setTimeout(() => {
        suppressCommitRef.current = false;
      }, 0);
    },
    [applyLocalSnapshot],
  );

  // Restoring a snapshot is a side effect, so it is performed here rather than
  // inside a `setState` updater (which React may invoke more than once).
  const onUndo = useCallback(() => {
    // Fold any in-flight gesture into history first, so a drag followed by
    // Ctrl+Z undoes the drag rather than the edit before it.
    const base = flushCommit();
    if (!historyCanUndo(base)) return;
    const next = historyUndo(base);
    liveRef.current.history = next;
    setHistory(next);
    restoreSnapshot(next.present, true);
  }, [flushCommit, restoreSnapshot]);

  const onRedo = useCallback(() => {
    const base = liveRef.current.history;
    if (!historyCanRedo(base)) return;
    const next = historyRedo(base);
    liveRef.current.history = next;
    setHistory(next);
    restoreSnapshot(next.present, true);
  }, [restoreSnapshot]);

  // -------------------------------------------------------------------------
  // A/B compare — compares the complete rig state, not one module
  // -------------------------------------------------------------------------

  const onSelectAb = useCallback(
    (slot: AbSlot) => {
      if (liveRef.current.ab.active === slot) return;
      flushCommit();
      const next = setActive(liveRef.current.ab, slot, currentSnapshot());
      liveRef.current.ab = next;
      setAb(next);
      restoreSnapshot(activeSnapshot(next), true);
    },
    [currentSnapshot, flushCommit, restoreSnapshot],
  );

  const onCopyAb = useCallback(() => {
    const next = copyToOther(liveRef.current.ab, currentSnapshot());
    liveRef.current.ab = next;
    setAb(next);
  }, [currentSnapshot]);

  // -------------------------------------------------------------------------
  // Snapshots — instant bypass/param recall within the current preset
  // -------------------------------------------------------------------------

  /** Recall a slot: bypass + parameter values only, model/path untouched. */
  const recallSnapshot = useCallback(
    (index: number) => {
      const live = liveRef.current;
      if (index === live.snapshots.active) return;
      flushCommit();
      const nextBank = setActiveBankSlot(live.snapshots, index);
      liveRef.current.snapshots = nextBank;
      setSnapshots(nextBank);
      const merged = mergeSnapshotIntoRig(currentSnapshot(), activeBankSlot(nextBank));
      restoreSnapshot(merged, true);
    },
    [currentSnapshot, flushCommit, restoreSnapshot],
  );

  /** "Save Current Here" — overwrite one slot's bypass/params, keep its name. */
  const saveCurrentToSnapshot = useCallback(
    (index: number) => {
      const live = liveRef.current;
      const rig = currentSnapshot();
      const name = live.snapshots.slots[index]?.name ?? String(index + 1);
      const next = saveBankSlot(live.snapshots, index, snapshotContentFromRig(rig, name));
      liveRef.current.snapshots = next;
      setSnapshots(next);
      markDirty();
    },
    [currentSnapshot, markDirty],
  );

  const renameSnapshot = useCallback((index: number, name: string) => {
    const live = liveRef.current;
    const slot = live.snapshots.slots[index];
    if (!slot) return;
    const next = saveBankSlot(live.snapshots, index, { ...slot, name });
    liveRef.current.snapshots = next;
    setSnapshots(next);
  }, []);

  // -------------------------------------------------------------------------
  // Preset handling
  // -------------------------------------------------------------------------

  const commitLoadPreset = useCallback(
    (id: string, opts?: { discardCurrent?: boolean }) => {
      const live = liveRef.current;
      let nextDrafts = live.drafts;

      if (opts?.discardCurrent) {
        nextDrafts = { ...live.drafts };
        delete nextDrafts[live.currentPresetId];
        setDrafts(nextDrafts);
        setDirtyPresetIds((prev) => {
          if (!prev.has(live.currentPresetId)) return prev;
          const next = new Set(prev);
          next.delete(live.currentPresetId);
          return next;
        });
      }

      const snap =
        nextDrafts[id] ?? live.savedRigs[id] ?? factorySnapshot(id);
      if (!snap) return;

      // A preset load is a new baseline, not an edit: history, both A/B
      // slots and the snapshot bank all restart from it. Factory/saved-rig
      // presets never carry a snapshot bank of their own (only a preset
      // *file* can — see `loadPresetFile`), so this always reseeds fresh.
      suppressCommitRef.current = true;
      applyLocalSnapshot(snap, id, !!nextDrafts[id]);
      const freshHistory = historyReset(live.history, snap);
      const freshAb = createAb(snap);
      const freshSnapshots = defaultSnapshotBank(snap);
      liveRef.current.history = freshHistory;
      liveRef.current.ab = freshAb;
      liveRef.current.snapshots = freshSnapshots;
      setHistory(freshHistory);
      setAb(freshAb);
      setSnapshots(freshSnapshots);
      setPendingSwitchId(null);
      window.setTimeout(() => {
        suppressCommitRef.current = false;
      }, 0);
    },
    [applyLocalSnapshot],
  );

  const loadPreset = useCallback(
    (id: string) => {
      const live = liveRef.current;
      if (id === live.currentPresetId) return;

      // Leaving a dirty unsaved rig → ask before switching.
      if (live.modified) {
        setPendingSwitchId(id);
        return;
      }

      commitLoadPreset(id);
    },
    [commitLoadPreset],
  );

  /// A preset loaded from a sidebar file: same new-baseline semantics as
  /// `commitLoadPreset`, but the snapshot comes from disk, not presetsData.
  const loadPresetFile = useCallback(
    (file: PresetFile) => {
      suppressCommitRef.current = true;
      applyLocalSnapshot(file.snapshot, file.id, false);
      const freshHistory = historyReset(liveRef.current.history, file.snapshot);
      const freshAb = createAb(file.snapshot);
      const freshSnapshots = normalizeSnapshotBank(file.snapshots, file.snapshot);
      liveRef.current.history = freshHistory;
      liveRef.current.ab = freshAb;
      liveRef.current.snapshots = freshSnapshots;
      setHistory(freshHistory);
      setAb(freshAb);
      setSnapshots(freshSnapshots);
      setPendingSwitchId(null);
      window.setTimeout(() => {
        suppressCommitRef.current = false;
      }, 0);
    },
    [applyLocalSnapshot],
  );

  /// Sidebar "＋ Save preset": user preset ids are `U` + base36 timestamp so
  /// files never collide with the factory `NNX` ids.
  const buildSavePayload = useCallback(
    (name: string) => {
      const snap = currentSnapshot();
      const id = `U${Date.now().toString(36).toUpperCase().slice(-4)}`;
      return {
        fileName: presetFileName(id, name),
        content: serializePreset(id, name, snap.activeCat, snap, liveRef.current.snapshots),
      };
    },
    [currentSnapshot],
  );


  const stepPreset = useCallback(
    (dir: number) => {
      const idx = presetsData.findIndex(
        (p) => p.id === liveRef.current.currentPresetId,
      );
      const next = (idx + dir + presetsData.length) % presetsData.length;
      loadPreset(presetsData[next]!.id);
    },
    [loadPreset],
  );

  // -------------------------------------------------------------------------
  // Edits
  // -------------------------------------------------------------------------

  const selectCategory = useCallback(
    (cat: CategoryId) => {
      setActiveCat(cat);
      const modelId =
        liveRef.current.stageModels[cat] || models[cat]?.[0]?.id;
      if (!modelId) return;
      setActiveModelId(modelId);
    },
    [],
  );

  const selectModel = useCallback(
    (id: string) => {
      const cat = liveRef.current.activeCat;
      const selectedParams =
        liveRef.current.parameters[id] ??
        parameterDefaults[id]?.map((param) => ({ ...param })) ??
        [];
      setActiveModelId(id);
      setStageModels((prev) => ({ ...prev, [cat]: id }));
      if (liveRef.current.parameters[id] === undefined && selectedParams.length > 0) {
        setParameters((prev) => ({ ...prev, [id]: selectedParams }));
      }
      postModel(categories[cat].node, id);
      for (const param of selectedParams) {
        postParam(param.id, param.val);
      }
      flushParamEditsNow();
      markDirty();
    },
    [markDirty],
  );

  const toggleBypassFor = useCallback(
    (cat: CategoryId) => {
      const nextBypass = !liveRef.current.bypassed[cat];
      const nextBypassed = {
        ...liveRef.current.bypassed,
        [cat]: nextBypass,
      };
      // Mirror synchronously so two rapid clicks cannot both derive from the
      // same pre-render value.
      liveRef.current.bypassed = nextBypassed;
      setBypassed(nextBypassed);
      postEnabled(categories[cat].node, !nextBypass);
      flushParamEditsNow();
      markDirty();
    },
    [markDirty],
  );

  const toggleBypass = useCallback(
    () => toggleBypassFor(liveRef.current.activeCat),
    [toggleBypassFor],
  );

  const toggleGlobalBypass = useCallback(() => {
    const next = {
      ...liveRef.current.globals,
      globalBypass: !liveRef.current.globals.globalBypass,
    };
    liveRef.current.globals = next;
    setGlobals(next);
    postParam(POWER_PARAM_ID, next.globalBypass ? 0 : 1);
    flushParamEditsNow();
    markDirty();
  }, [markDirty]);

  const onGlobalParamChange = useCallback(
    (id: string, value: number) => {
      postParam(id, value);
      setGlobals((prev) => {
        if (id === "input_trim") return { ...prev, inputTrim: value };
        if (id === "output_trim") return { ...prev, outputTrim: value };
        return prev;
      });
      markDirty();
    },
    [markDirty],
  );

  const loadNamCapture = useCallback(
    (json: string, opts: NamCaptureLoadOptions) => {
      postLoadNamCapture(json, opts);
      markDirty();
    },
    [markDirty],
  );

  /// NAM tab click: file text was read natively; route it into the amp slot
  /// with sensible defaults (the ModuleEditor picker still offers
  /// stereo/full-rig fine control).
  const loadNamFile = useCallback(
    (name: string, json: string) => {
      loadNamCapture(json, { name, stereo: true, fullRig: false });
    },
    [loadNamCapture],
  );

  const prepareNamEngine = useCallback(() => {
    setStageModels((prev) => ({ ...prev, amp: "nam_capture" }));
    if (liveRef.current.activeCat === "amp") setActiveModelId("nam_capture");
    postModel(categories.amp.node, "nam_capture");
    flushParamEditsNow();
    markDirty();
  }, [markDirty]);

  /// A successful IR load switches the Cabinet slot to the convolution
  /// engine — the user clicked an IR to hear it, not to park it. The DSP
  /// keeps the loaded IR either way, so switching back to a modeled voicing
  /// (and back again) never needs a reload.
  const onIrLoaded = useCallback(() => {
    setStageModels((prev) => ({ ...prev, cab: "ir" }));
    if (liveRef.current.activeCat === "cab") setActiveModelId("ir");
    postModel(categories.cab.node, "ir");
    flushParamEditsNow();
    markDirty();
  }, [markDirty]);

  const bypassCab = useCallback(() => {
    postEnabled(categories.cab.node, false);
    flushParamEditsNow();
    setBypassed((prev) => ({ ...prev, cab: true }));
    markDirty();
  }, [markDirty]);

  const reorderPath = useCallback(
    (next: CategoryId[]) => {
      setPathOrder(next);
      postPathOrder(next);
      markDirty();
      const live = liveRef.current;
      if (!next.includes(live.activeCat)) {
        const fallback = next[0] ?? rackFromPath(next)[0];
        if (fallback) {
          setActiveCat(fallback);
          const modelId =
            live.stageModels[fallback] || models[fallback]?.[0]?.id;
          if (modelId) {
            setActiveModelId(modelId);
            postModel(categories[fallback].node, modelId);
          }
        }
      }
      flushParamEditsNow();
    },
    [markDirty],
  );

  const onParamChange = useCallback(
    (id: string, value: number) => {
      postParam(id, value);
      markDirty();
      const modelId = liveRef.current.activeModelId;
      setParameters((prev) => {
        const modelParams = prev[modelId] ?? parameterDefaults[modelId];
        if (!modelParams) return prev;
        return {
          ...prev,
          [modelId]: modelParams.map((p) =>
            p.id === id ? { ...p, val: value } : p,
          ),
        };
      });
    },
    [markDirty],
  );

  // -------------------------------------------------------------------------
  // Block menu actions
  // -------------------------------------------------------------------------

  const copySettings = useCallback((cat: CategoryId) => {
    const live = liveRef.current;
    const modelId = live.stageModels[cat];
    if (!modelId) return;
    setClipboard({
      cat,
      modelId,
      params: (live.parameters[modelId] ?? []).map((p) => ({ ...p })),
    });
  }, []);

  const pasteSettings = useCallback(
    (cat: CategoryId) => {
      const clip = clipboard;
      // Only paste within the same category: models in different categories
      // have entirely different parameter sets.
      if (!clip || clip.cat !== cat) return;
      const live = liveRef.current;

      setStageModels((prev) => ({ ...prev, [cat]: clip.modelId }));
      setParameters((prev) => ({
        ...prev,
        [clip.modelId]: clip.params.map((p) => ({ ...p })),
      }));
      if (live.activeCat === cat) setActiveModelId(clip.modelId);

      postModel(categories[cat].node, clip.modelId);
      for (const p of clip.params) postParam(p.id, p.val);
      flushParamEditsNow();
      markDirty();
    },
    [clipboard, markDirty],
  );

  const resetModule = useCallback(
    (cat: CategoryId) => {
      const live = liveRef.current;
      const modelId = live.stageModels[cat];
      const defaults = modelId ? parameterDefaults[modelId] : undefined;
      if (!modelId || !defaults) return;

      setParameters((prev) => ({
        ...prev,
        [modelId]: defaults.map((p) => ({ ...p })),
      }));
      for (const p of defaults) postParam(p.id, p.val);
      flushParamEditsNow();
      markDirty();
    },
    [markDirty],
  );

  // -------------------------------------------------------------------------
  // Save / revert
  // -------------------------------------------------------------------------

  const saveRig = useCallback(() => {
    const live = liveRef.current;
    const snap = makeSnapshot(
      live.activeCat,
      live.activeModelId,
      live.stageModels,
      live.pathOrder,
      live.bypassed,
      live.parameters,
      live.globals,
    );
    setSavedRigs((prev) => ({ ...prev, [live.currentPresetId]: snap }));
    setDrafts((prev) => {
      if (!(live.currentPresetId in prev)) return prev;
      const next = { ...prev };
      delete next[live.currentPresetId];
      return next;
    });
    setModified(false);
    setDirtyPresetIds((prev) => {
      if (!prev.has(live.currentPresetId)) return prev;
      const next = new Set(prev);
      next.delete(live.currentPresetId);
      return next;
    });
  }, []);

  const confirmSaveAndSwitch = useCallback(() => {
    const target = liveRef.current.pendingSwitchId;
    saveRig();
    if (target) commitLoadPreset(target);
  }, [commitLoadPreset, saveRig]);

  const confirmDiscardAndSwitch = useCallback(() => {
    const target = liveRef.current.pendingSwitchId;
    if (!target) {
      setPendingSwitchId(null);
      return;
    }
    commitLoadPreset(target, { discardCurrent: true });
  }, [commitLoadPreset]);

  const cancelSwitch = useCallback(() => setPendingSwitchId(null), []);

  const revertRig = useCallback(() => {
    const live = liveRef.current;
    const snap =
      live.savedRigs[live.currentPresetId] ??
      factorySnapshot(live.currentPresetId);
    if (!snap) return;
    setDrafts((prev) => {
      if (!(live.currentPresetId in prev)) return prev;
      const next = { ...prev };
      delete next[live.currentPresetId];
      return next;
    });
    setDirtyPresetIds((prev) => {
      if (!prev.has(live.currentPresetId)) return prev;
      const next = new Set(prev);
      next.delete(live.currentPresetId);
      return next;
    });
    suppressCommitRef.current = true;
    applyLocalSnapshot(snap, live.currentPresetId, false);
    const freshHistory = historyReset(live.history, snap);
    const freshSnapshots = defaultSnapshotBank(snap);
    liveRef.current.history = freshHistory;
    liveRef.current.snapshots = freshSnapshots;
    setHistory(freshHistory);
    setSnapshots(freshSnapshots);
    window.setTimeout(() => {
      suppressCommitRef.current = false;
    }, 0);
  }, [applyLocalSnapshot]);

  // -------------------------------------------------------------------------
  // Browser-only Test DI preview
  // -------------------------------------------------------------------------

  const toggleTest = useCallback(async () => {
    if (!audioRef.current) {
      const Ctx =
        window.AudioContext ||
        (window as unknown as { webkitAudioContext: typeof AudioContext })
          .webkitAudioContext;
      const ctx = new Ctx();
      const gain = ctx.createGain();
      gain.connect(ctx.destination);
      audioRef.current = { ctx, gain, timer: null };
    }

    const audio = audioRef.current;
    const next = !testing;
    setTesting(next);

    if (audio.timer !== null) {
      window.clearInterval(audio.timer);
      audio.timer = null;
    }

    if (!next) {
      releasePreview();
      return;
    }

    audio.timer = window.setInterval(() => {
      if (audio.ctx.state === "suspended") void audio.ctx.resume();
      [82.41, 110.0, 146.83, 196.0].forEach((f, i) => {
        const osc = audio.ctx.createOscillator();
        const g = audio.ctx.createGain();
        osc.type = "triangle";
        osc.frequency.setValueAtTime(f, audio.ctx.currentTime + i * 0.04);
        g.gain.setValueAtTime(0, audio.ctx.currentTime);
        g.gain.linearRampToValueAtTime(
          0.16,
          audio.ctx.currentTime + 0.01 + i * 0.04,
        );
        g.gain.exponentialRampToValueAtTime(
          0.001,
          audio.ctx.currentTime + 0.75 + i * 0.04,
        );
        osc.connect(g);
        g.connect(audio.gain);
        osc.start();
        osc.stop(audio.ctx.currentTime + 0.85);
      });
    }, 850);
  }, [testing]);

  // Preview meter animation. Runs only while the browser Test DI is active and
  // no native host is supplying real telemetry.
  useEffect(() => {
    if (!testing) return;
    let alive = true;
    let timer = 0;
    let level = 0;

    const tick = () => {
      if (!alive) return;
      timer = window.setTimeout(tick, 33);
      level = Math.max(level * 0.94, Math.random() < 0.06 ? 0.55 + Math.random() * 0.3 : 0);
      const out = level * 0.85;
      pushSimulatedFrame({
        inPeak: level,
        inRms: level * 0.62,
        outPeak: out,
        outRms: out * 0.62,
        inClip: false,
        outClip: false,
      });
    };
    tick();

    return () => {
      alive = false;
      window.clearTimeout(timer);
      releasePreview();
    };
  }, [testing]);

  useEffect(() => {
    return () => {
      const audio = audioRef.current;
      if (!audio) return;
      if (audio.timer !== null) window.clearInterval(audio.timer);
      void audio.ctx.close();
    };
  }, []);

  // -------------------------------------------------------------------------
  // Shortcuts
  // -------------------------------------------------------------------------

  useEffect(() => {
    const isTypingTarget = (el: EventTarget | null) => {
      if (!(el instanceof HTMLElement)) return false;
      const tag = el.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return true;
      return el.isContentEditable;
    };

    const onKeyDown = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.altKey) return;
      if (isTypingTarget(e.target)) return;

      if (liveRef.current.pendingSwitchId) {
        if (e.key === "Escape") {
          e.preventDefault();
          cancelSwitch();
        }
        return;
      }

      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") {
        e.preventDefault();
        if (liveRef.current.modified) saveRig();
        return;
      }
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "z") {
        e.preventDefault();
        if (e.shiftKey) onRedo();
        else onUndo();
        return;
      }
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "y") {
        e.preventDefault();
        onRedo();
        return;
      }
      if (e.ctrlKey || e.metaKey) return;

      if (e.key === "ArrowLeft") {
        e.preventDefault();
        stepPreset(-1);
        return;
      }
      if (e.key === "ArrowRight") {
        e.preventDefault();
        stepPreset(1);
        return;
      }
      if (e.key === " " || e.code === "Space") {
        const target = e.target as HTMLElement | null;
        if (
          target &&
          (target.tagName === "INPUT" ||
            target.tagName === "TEXTAREA" ||
            target.isContentEditable)
        ) {
          return;
        }
        // DAW transport owns bare Space on the editor surface. Forward so
        // play/pause still works when the native claim path misses an OSR
        // focus edge case. Never steal Space for bypass.
        e.preventDefault();
        postGlobalCommand("transport:play-pause");
        return;
      }
      if (e.key >= "1" && e.key <= "9") {
        const idx = Number(e.key) - 1;
        const path = liveRef.current.pathOrder;
        const cat = path[idx] ?? chainOrder[idx];
        if (cat) {
          e.preventDefault();
          selectCategory(cat);
        }
      }
    };

    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [
    cancelSwitch,
    onRedo,
    onUndo,
    saveRig,
    selectCategory,
    stepPreset,
  ]);

  const pendingName =
    presetsData.find((p) => p.id === currentPresetId)?.name ?? "This rig";

  return (
    <Layout
      currentPresetId={currentPresetId}
      presetName={currentPreset.name}
      modified={modified}
      dirtyPresetIds={dirtyPresetIds}
      activeCat={activeCat}
      activeModelId={activeModelId}
      stageModels={stageModels}
      pathOrder={pathOrder}
      bypassed={bypassed}
      params={params}
      testing={testing}
      showTestDi={showTestDi}
      inputTrim={globals.inputTrim}
      outputTrim={globals.outputTrim}
      globalBypass={globals.globalBypass}
      canUndo={historyCanUndo(history)}
      canRedo={historyCanRedo(history)}
      abSlot={ab.active}
      snapshotSlots={snapshots.slots}
      activeSnapshotIndex={snapshots.active}
      onSelectSnapshot={recallSnapshot}
      onSaveSnapshot={saveCurrentToSnapshot}
      onRenameSnapshot={renameSnapshot}
      clipboardCat={clipboard?.cat ?? null}
      discardPrompt={
        pendingSwitchId
          ? {
              presetName: pendingName,
              onSave: confirmSaveAndSwitch,
              onDiscard: confirmDiscardAndSwitch,
              onCancel: cancelSwitch,
            }
          : null
      }
      onUndo={onUndo}
      onRedo={onRedo}
      onSelectAb={onSelectAb}
      onCopyAb={onCopyAb}
      onStepPreset={stepPreset}
      onLoadPresetFile={loadPresetFile}
      buildSavePayload={buildSavePayload}
      buildFactorySnapshot={factorySnapshot}
      onLoadNamFile={loadNamFile}
      onPrepareNamEngine={prepareNamEngine}
      onIrLoaded={onIrLoaded}
      onToggleTest={() => void toggleTest()}
      onSave={saveRig}
      onRevert={revertRig}
      onSelectCategory={selectCategory}
      onToggleModule={toggleBypassFor}
      onReorderPath={reorderPath}
      onSelectModel={selectModel}
      onToggleBypass={toggleBypass}
      onToggleGlobalBypass={toggleGlobalBypass}
      onParamChange={onParamChange}
      onGlobalParamChange={onGlobalParamChange}
      onCopySettings={copySettings}
      onPasteSettings={pasteSettings}
      onResetModule={resetModule}
      onLoadNamCapture={loadNamCapture}
      onBypassCab={bypassCab}
    />
  );
}

/** Empty state: no compatible instance is currently bound (route `/`, or a
 * removed active instance with nothing left to fall back to). Never leaves
 * a destroyed insert's controls on screen. */
function NoInstanceSelected() {
  return (
    <div className="flex h-screen w-screen items-center justify-center bg-neutral-950 text-sm text-neutral-400">
      No Rodhareist instances are available in this project.
    </div>
  );
}

/**
 * Mounts the real editor only once a native-approved instance is bound, and
 * remounts it (via `key`) on every instance switch.
 *
 * This stands in for the spec's "centralized bound-instance store with
 * atomic replacement" until Phase 5 gives instances real, persisted DSP
 * state: with nothing to preserve across a switch yet, a full remount gives
 * the same guarantees that section actually cares about — no stale timers
 * (`commitTimerRef`), no mixed state from two instances, no leaked
 * optimistic edits — for free, via each hook's own unmount cleanup. Once
 * Phase 5 lands, the natural next step is to replace this remount with a
 * store that carries the previous instance's confirmed state out and the
 * next instance's snapshot in, instead of discarding and starting fresh.
 */
function BoundEditor() {
  const { instanceId, connectionStatus, state } = useBoundInstance();
  // Map the native state blob once per binding; `null` (fresh insert / no
  // valid blob) makes the editor establish the factory initial instead.
  const boundSnapshot = useMemo(
    () => snapshotFromRodhareistState(state),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [instanceId, state],
  );
  if (connectionStatus !== "active" || !instanceId) {
    return <NoInstanceSelected />;
  }
  return <RodhareistEditor key={instanceId} boundSnapshot={boundSnapshot} />;
}

function AppRoot() {
  if (import.meta.env.DEV) {
    return <RodhareistEditor />;
  }

  return (
    <HashRouter>
      <BoundInstanceProvider>
        <Routes>
          <Route path="/" element={<NoInstanceSelected />} />
          <Route path="/instance/:instanceId" element={<BoundEditor />} />
        </Routes>
      </BoundInstanceProvider>
    </HashRouter>
  );
}

const rootEl = document.getElementById("root");
if (rootEl) {
  createRoot(rootEl).render(
    <StrictMode>
      <AppRoot />
    </StrictMode>,
  );
}
