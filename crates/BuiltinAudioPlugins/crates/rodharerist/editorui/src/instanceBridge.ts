// Native<->React instance-binding bridge protocol.
//
// Separate from `bridge.ts` (which forwards parameter/model/meter calls
// through an injected `window.rodhareist` object — a different, still-inert
// contract; see its own file header). This one binds *which* DSP insert the
// shared CEF page is currently showing, and travels over a different
// transport:
//
//   native -> React : `window.postMessage(msg, "*")`, run via
//                      `Frame::execute_java_script` (see
//                      `builtin_plugin_editor_window.rs::push_selected_instance`)
//   React -> native : `fetch("__bridge", { method: "POST", body: JSON })`,
//                      intercepted by the `mikoplugin://` scheme handler
//                      before it reaches any plugin's asset resolver (see
//                      `SphereWebView/src/scheme.rs`)
//
// Message shapes here must match the Rust structs in
// `builtin_plugin_editor_window.rs` (`SelectInstanceMsg`, `InstanceRemovedMsg`,
// `InboundMsg`) field-for-field — both sides serialize with camelCase.

/** Bump alongside any breaking change to a message shape below. */
export const BRIDGE_PROTOCOL_VERSION = 1;

/** Relative to the page's own origin (`mikoplugin://<plugin>/`). */
const BRIDGE_ENDPOINT = "__bridge";

export type InstanceDisplayMetadata = {
  trackId: string;
  trackName: string;
  insertId: string;
  insertName: string;
};

/** Native -> React: rebind the shared page to a different DSP instance. */
export type SelectInstanceMessage = {
  type: "futureboard.selectInstance";
  protocolVersion: number;
  pluginId: string;
  instanceId: string;
  bindingGeneration: number;
  display: InstanceDisplayMetadata;
  stateRevision: number;
  /**
   * The bound insert's persisted DSP state (native's `decode_state_bytes`) —
   * a real `RodhareistState` blob for an insert with something to restore,
   * `{}` for a fresh insert. Untyped here because it is untrusted
   * boundary data: `stateMap.ts`'s `snapshotFromRodhareistState` validates
   * its shape structurally before use.
   */
  state: unknown;
};

/** Native -> React: the instance that used to be selected no longer exists. */
export type InstanceRemovedMessage = {
  type: "futureboard.instanceRemoved";
  protocolVersion: number;
  instanceId: string;
};

/** Native -> React: one ~30 Hz telemetry frame for the bound instance. */
export type MetersMessage = {
  type: "futureboard.meters";
  protocolVersion: number;
  instanceId: string;
  inPeak: number;
  inRms: number;
  outPeak: number;
  outRms: number;
  inClip: boolean;
  outClip: boolean;
};

/** Native -> React: low-rate footer status from the host's shared region. */
export type HostStatusMessage = {
  type: "futureboard.hostStatus";
  protocolVersion: number;
  instanceId: string;
  sampleRate: number;
  blockSize: number;
  latencySamples: number;
};

/** Native -> React: async outcome of a `loadNamCapture` request. */
export type NamCaptureResultMessage = {
  type: "futureboard.namCaptureResult";
  protocolVersion: number;
  instanceId: string;
  ok: boolean;
  name: string;
  error?: string | null;
  receptiveField: number;
  fullRig: boolean;
  architecture?: string;
  family?: string;
  slimmable?: boolean;
  submodelCount?: number;
};

/** Native -> React: async outcome of a `loadIr` request. */
export type IrLoadResultMessage = {
  type: "futureboard.irLoadResult";
  protocolVersion: number;
  instanceId: string;
  ok: boolean;
  name: string;
  error?: string | null;
  /** Frames actually convolved, at the engine's rate (0 on failure). */
  frames: number;
  /** Latency the convolution adds, in samples (0 on failure). */
  latencySamples: number;
  /** The file carried two distinct channels (true-stereo IR). */
  stereo: boolean;
  /** The file was longer than the engine's cap and got cut. */
  truncated: boolean;
};

/** Which per-plugin user folder a file message targets. */
export type FileKind = "presets" | "irs" | "nams";

export type FileEntry = {
  fileName: string;
  sizeBytes: number;
  modifiedMs: number;
};

/** Native -> React: one kind's user-file listing (rebuilt wholesale). */
export type FileListMessage = {
  type: "futureboard.fileList";
  protocolVersion: number;
  kind: FileKind;
  files: FileEntry[];
};

/** Native -> React: one user file's text content (or the failure). */
export type FileContentMessage = {
  type: "futureboard.fileContent";
  protocolVersion: number;
  kind: FileKind;
  fileName: string;
  ok: boolean;
  content?: string | null;
  error?: string | null;
};

/** Native -> React: outcome of a `writeFile`. */
export type FileWrittenMessage = {
  type: "futureboard.fileWritten";
  protocolVersion: number;
  kind: FileKind;
  fileName: string;
  ok: boolean;
  error?: string | null;
};

export type Tone3000ToneCard = {
  id: number;
  title: string;
  creator: string;
  gear: string;
  format: string;
  image?: string | null;
};

export type Tone3000StatusMessage = {
  type: "futureboard.tone3000Status";
  protocolVersion: number;
  configured: boolean;
  error?: string | null;
};

export type Tone3000SearchResultMessage = {
  type: "futureboard.tone3000SearchResult";
  protocolVersion: number;
  ok: boolean;
  query: string;
  page: number;
  tones: Tone3000ToneCard[];
  error?: string | null;
};

export type Tone3000LoadResultMessage = {
  type: "futureboard.tone3000LoadResult";
  protocolVersion: number;
  ok: boolean;
  toneId: number;
  name: string;
  fileName?: string | null;
  error?: string | null;
};

export type NativeMessage =
  | SelectInstanceMessage
  | InstanceRemovedMessage
  | MetersMessage
  | HostStatusMessage
  | NamCaptureResultMessage
  | IrLoadResultMessage
  | FileListMessage
  | FileContentMessage
  | FileWrittenMessage
  | Tone3000StatusMessage
  | Tone3000SearchResultMessage
  | Tone3000LoadResultMessage;

const NATIVE_MESSAGE_TYPES = new Set([
  "futureboard.selectInstance",
  "futureboard.instanceRemoved",
  "futureboard.meters",
  "futureboard.hostStatus",
  "futureboard.namCaptureResult",
  "futureboard.irLoadResult",
  "futureboard.fileList",
  "futureboard.fileContent",
  "futureboard.fileWritten",
  "futureboard.tone3000Status",
  "futureboard.tone3000SearchResult",
  "futureboard.tone3000LoadResult",
]);

function isNativeMessage(data: unknown): data is NativeMessage {
  if (!data || typeof data !== "object") return false;
  const type = (data as { type?: unknown }).type;
  return typeof type === "string" && NATIVE_MESSAGE_TYPES.has(type);
}

/**
 * Subscribe to native->React bridge messages. Returns an unsubscribe
 * function. Safe to call with no native host present (e.g. `bun run dev`) —
 * the listener just never fires.
 */
export function onNativeMessage(handler: (msg: NativeMessage) => void): () => void {
  const listener = (event: MessageEvent) => {
    if (isNativeMessage(event.data)) handler(event.data);
  };
  window.addEventListener("message", listener);
  return () => window.removeEventListener("message", listener);
}

/** Fire-and-forget POST to the native bridge endpoint. Never throws. */
function post(body: unknown): void {
  try {
    void fetch(BRIDGE_ENDPOINT, {
      method: "POST",
      body: JSON.stringify(body),
    }).catch(() => {
      // No native host (standalone browser preview) — expected, not an error.
    });
  } catch {
    /* no-op */
  }
}

/** Sent once on mount. Native replies with the currently-selected instance
 * (or nothing, if none is selected yet) once this arrives. */
export function sendBridgeReady(pluginId: string): void {
  post({
    type: "futureboard.bridgeReady",
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    pluginId,
    bridgeVersion: BRIDGE_PROTOCOL_VERSION,
  });
}

/** Acknowledges a `selectInstance` once its state has been atomically applied. */
export function sendInstanceReady(
  pluginId: string,
  instanceId: string,
  bindingGeneration: number,
  stateRevision: number,
): void {
  post({
    type: "futureboard.instanceReady",
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    pluginId,
    instanceId,
    bindingGeneration,
    stateRevision,
  });
}

/**
 * Ask native to validate and (if approved) select `instanceId`. Used when the
 * route changed without an approved `selectInstance` behind it (manual URL
 * edit, browser back/forward) — the route is never trusted on its own.
 */
export function requestSelectInstance(instanceId: string): void {
  post({
    type: "futureboard.requestSelectInstance",
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    instanceId,
  });
}

// ---------------------------------------------------------------------------
// Live parameter edits (`futureboard.setParams`)
//
// Native validates every batch against the *current* binding generation, so
// each post carries the identity of the selection it was made under. The
// module-level binding below is the write-side mirror of what
// `BoundInstanceProvider` renders — set on every approved `selectInstance`,
// cleared when that instance goes away.

export type ActiveParamBinding = {
  pluginId: string;
  instanceId: string;
  bindingGeneration: number;
};

let activeParamBinding: ActiveParamBinding | null = null;

/** Listeners that must drop pending work when the binding changes (e.g. the
 * coalescing buffer in `bridge.ts` — edits queued against the old instance
 * must never flush against the new one). */
const bindingResetListeners = new Set<() => void>();

export function onParamBindingReset(listener: () => void): () => void {
  bindingResetListeners.add(listener);
  return () => bindingResetListeners.delete(listener);
}

function notifyBindingReset(): void {
  for (const listener of bindingResetListeners) listener();
}

/** Called by `BoundInstanceProvider` on every approved `selectInstance`. */
export function setActiveParamBinding(binding: ActiveParamBinding): void {
  activeParamBinding = binding;
  notifyBindingReset();
}

/** Called when the bound instance is removed (or the page unbinds). */
export function clearActiveParamBinding(): void {
  activeParamBinding = null;
  notifyBindingReset();
}

/** Test-only view of the current binding. */
export function getActiveParamBinding(): ActiveParamBinding | null {
  return activeParamBinding;
}

export type ParamEdit = { id: string; value: number };

// --- Plugin user files (Documents/Futureboard Studio/<plugin>/...) ---------
// File ops are plugin-global (the folders belong to the plugin, not an
// insert); they still require an active binding so a detached dev preview
// stays a no-op.

export function postListFiles(kind: FileKind): void {
  const binding = activeParamBinding;
  if (!binding) return;
  post({
    type: "futureboard.listFiles",
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    pluginId: binding.pluginId,
    kind,
  });
}

export function postReadFile(kind: FileKind, fileName: string): void {
  const binding = activeParamBinding;
  if (!binding) return;
  post({
    type: "futureboard.readFile",
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    pluginId: binding.pluginId,
    kind,
    fileName,
  });
}

export function postWriteFile(kind: FileKind, fileName: string, content: string): void {
  const binding = activeParamBinding;
  if (!binding) return;
  post({
    type: "futureboard.writeFile",
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    pluginId: binding.pluginId,
    kind,
    fileName,
    content,
  });
}

/**
 * Post a `.nam` capture load for the bound instance. `json` is the raw file
 * text (read client-side via `FileReader`); it rides the POST body to native
 * and on to the plugin-host process. Silent no-op when nothing is bound.
 * The result arrives asynchronously as a `futureboard.namCaptureResult`
 * native message.
 */
export function postLoadNamCaptureForBoundInstance(
  json: string,
  opts: { name: string; stereo: boolean; fullRig: boolean },
): void {
  const binding = activeParamBinding;
  if (!binding) return;
  post({
    type: "futureboard.loadNamCapture",
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    pluginId: binding.pluginId,
    instanceId: binding.instanceId,
    bindingGeneration: binding.bindingGeneration,
    name: opts.name,
    json,
    stereo: opts.stereo,
    fullRig: opts.fullRig,
  });
}

/**
 * Ask native to load `fileName` from the plugin's IRs folder into the bound
 * instance's cabinet slot. Unlike `loadNamCapture` this carries only the name:
 * a `.wav` is binary, and native already owns that folder, so the bytes never
 * cross the webview. Silent no-op when nothing is bound. The result arrives
 * asynchronously as a `futureboard.irLoadResult` native message.
 */
export function postLoadIrForBoundInstance(fileName: string): void {
  const binding = activeParamBinding;
  if (!binding) return;
  post({
    type: "futureboard.loadIr",
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    pluginId: binding.pluginId,
    instanceId: binding.instanceId,
    bindingGeneration: binding.bindingGeneration,
    fileName,
  });
}

export function postTone3000Status(): void {
  const binding = activeParamBinding;
  if (!binding) return;
  post({
    type: "futureboard.tone3000Status",
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    pluginId: binding.pluginId,
  });
}

export function postTone3000Search(query: string, page = 1): void {
  const binding = activeParamBinding;
  if (!binding) return;
  post({
    type: "futureboard.tone3000Search",
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    pluginId: binding.pluginId,
    query,
    page,
  });
}

export function postTone3000LoadTone(
  toneId: number,
  opts: { stereo: boolean; fullRig: boolean; size?: string },
): void {
  const binding = activeParamBinding;
  if (!binding) return;
  post({
    type: "futureboard.tone3000LoadTone",
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    pluginId: binding.pluginId,
    instanceId: binding.instanceId,
    bindingGeneration: binding.bindingGeneration,
    toneId,
    size: opts.size ?? "",
    stereo: opts.stereo,
    fullRig: opts.fullRig,
  });
}

/**
 * Post one batch of parameter edits for the currently bound instance.
 * Silent no-op when nothing is bound (standalone browser preview, or the
 * window between `instanceRemoved` and the next `selectInstance`).
 */
export function postSetParams(edits: ParamEdit[]): void {
  const binding = activeParamBinding;
  if (!binding || edits.length === 0) return;
  post({
    type: "futureboard.setParams",
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    pluginId: binding.pluginId,
    instanceId: binding.instanceId,
    bindingGeneration: binding.bindingGeneration,
    params: edits,
  });
}

/**
 * Ask the DAW for a workspace command (e.g. transport play/pause). Not tied to
 * a parameter binding so Space can still reach the studio with no instance yet.
 * Native dedupes against `OnPreKeyEvent` / shell capture when those already
 * claimed the physical key.
 */
export function postGlobalCommand(commandId: string): void {
  if (!commandId) return;
  post({
    type: "futureboard.globalCommand",
    commandId,
  });
}
