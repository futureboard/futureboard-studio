export const BRIDGE_PROTOCOL_VERSION = 1
export const PLUGIN_ID = 'mixstation'

export type MixStationParams = {
  power: boolean
  inputTrimDb: number
  filtersEnabled: boolean
  hpfHz: number
  lpfHz: number
  eqEnabled: boolean
  lowGainDb: number
  lowMidFreqHz: number
  lowMidGainDb: number
  highMidFreqHz: number
  highMidGainDb: number
  highGainDb: number
  compEnabled: boolean
  compThresholdDb: number
  compRatio: number
  compAttackMs: number
  compReleaseMs: number
  compMakeupDb: number
  satEnabled: boolean
  satDrivePct: number
  satCharacterPct: number
  widthEnabled: boolean
  widthPct: number
  outputTrimDb: number
  limiterEnabled: boolean
  limiterCeilingDb: number
  limiterReleaseMs: number
  slot1Module: number
  slot2Module: number
  slot3Module: number
  slot4Module: number
  slot5Module: number
  slot6Module: number
  filtersTrimDb: number
  eqTrimDb: number
  compTrimDb: number
  satTrimDb: number
  widthTrimDb: number
  limiterTrimDb: number
}

export type MeterFrame = {
  inPeak: number
  inRms: number
  outPeak: number
  outRms: number
  gainReductionDb: number
  inClip: boolean
  outClip: boolean
  /**
   * Level entering and leaving each rack position, in chain order. The native
   * side omits these entirely when every value is zero, so an empty array means
   * "this build published no per-stage telemetry" and the row meters stay dark
   * rather than reading a fabricated silence.
   */
  slotInPeak: readonly number[]
  slotOutPeak: readonly number[]
}

/**
 * One analyser frame from the host, published at its own ~30 Hz rate.
 *
 * The host captures this *before* the DSP runs, so it is the signal arriving at
 * the insert — which is what an EQ overlay wants to be drawn against. `bins`
 * are byte-quantised on the native side to keep the payload small; `floorDb`
 * and `ceilDb` carry the scale so the page never duplicates those constants.
 */
export type SpectrumFrame = {
  minHz: number
  maxHz: number
  floorDb: number
  ceilDb: number
  /** Log-spaced magnitude bins, 0 = floorDb, 255 = ceilDb. */
  bins: Uint8Array
}

export const defaults: MixStationParams = {
  power: true,
  inputTrimDb: 0,
  filtersEnabled: false,
  hpfHz: 30,
  lpfHz: 20_000,
  eqEnabled: false,
  lowGainDb: 0,
  lowMidFreqHz: 400,
  lowMidGainDb: 0,
  highMidFreqHz: 2_500,
  highMidGainDb: 0,
  highGainDb: 0,
  compEnabled: false,
  compThresholdDb: -18,
  compRatio: 4,
  compAttackMs: 10,
  compReleaseMs: 120,
  compMakeupDb: 0,
  satEnabled: false,
  satDrivePct: 0,
  satCharacterPct: 50,
  widthEnabled: false,
  widthPct: 100,
  outputTrimDb: 0,
  limiterEnabled: false,
  limiterCeilingDb: -0.3,
  limiterReleaseMs: 100,
  slot1Module: 0,
  slot2Module: 0,
  slot3Module: 0,
  slot4Module: 0,
  slot5Module: 0,
  slot6Module: 0,
  filtersTrimDb: 0,
  eqTrimDb: 0,
  compTrimDb: 0,
  satTrimDb: 0,
  widthTrimDb: 0,
  limiterTrimDb: 0,
}

export const BOOLEAN_KEYS = [
  'power',
  'filtersEnabled',
  'eqEnabled',
  'compEnabled',
  'satEnabled',
  'widthEnabled',
  'limiterEnabled',
] as const satisfies readonly (keyof MixStationParams)[]

export const NUMERIC_KEYS = [
  'inputTrimDb',
  'hpfHz',
  'lpfHz',
  'lowGainDb',
  'lowMidFreqHz',
  'lowMidGainDb',
  'highMidFreqHz',
  'highMidGainDb',
  'highGainDb',
  'compThresholdDb',
  'compRatio',
  'compAttackMs',
  'compReleaseMs',
  'compMakeupDb',
  'satDrivePct',
  'satCharacterPct',
  'widthPct',
  'outputTrimDb',
  'limiterCeilingDb',
  'limiterReleaseMs',
  'slot1Module',
  'slot2Module',
  'slot3Module',
  'slot4Module',
  'slot5Module',
  'slot6Module',
  'filtersTrimDb',
  'eqTrimDb',
  'compTrimDb',
  'satTrimDb',
  'widthTrimDb',
  'limiterTrimDb',
] as const satisfies readonly (keyof MixStationParams)[]

type Binding = {
  pluginId: string
  instanceId: string
  bindingGeneration: number
}

/**
 * Where this instance sits in the project. Native fills this in on every
 * `selectInstance`, and it is the only way an editor can name the insert it is
 * bound to — two inserts of the same plug-in are otherwise indistinguishable.
 */
export type InstanceDisplay = {
  trackId: string
  trackName: string
  insertId: string
  insertName: string
}

/** The approved binding, as handed to the editor. */
export type BoundInstance = Binding & { display: InstanceDisplay | null }

type SelectInstanceMessage = {
  type: 'futureboard.selectInstance'
  protocolVersion: number
  pluginId: string
  instanceId: string
  bindingGeneration: number
  /** Absent on a native build predating the metadata. */
  display?: unknown
  stateRevision: number
  state: unknown
}

type InstanceRemovedMessage = {
  type: 'futureboard.instanceRemoved'
  protocolVersion: number
  instanceId: string
  bindingGeneration: number
}

/**
 * The analyser message carries no `bindingGeneration` — the native side only
 * stamps it with the instance id, so that plus the protocol version is the full
 * freshness guard available here.
 */
type SpectrumMessage = {
  type: 'futureboard.spectrum'
  protocolVersion: number
  instanceId: string
  minHz: number
  maxHz: number
  floorDb: number
  ceilDb: number
  bins: number[]
}

type MetersMessage = Omit<MeterFrame, 'slotInPeak' | 'slotOutPeak'> & {
  type: 'futureboard.meters'
  /** Absent on a native build that predates per-stage telemetry. */
  slotInPeak?: unknown
  slotOutPeak?: unknown
  protocolVersion: number
  instanceId: string
  bindingGeneration: number
}

let binding: Binding | null = null
let stateRevision = -1
let latestBindingGeneration = -1
let latestGenerationRemoved = false
const pending = new Map<string, number>()
let scheduled = false

/// Event the dev preview host (`src/dev/previewHost.ts`) listens for. Never
/// dispatched in a production bundle: `import.meta.env.DEV` compiles to
/// `false` there.
export const DEV_POST_EVENT = 'futureboard:dev-post'

/// Whether this page is the dev-server preview rather than an embedded editor.
export const IS_BROWSER_PREVIEW =
  import.meta.env.DEV && typeof window !== 'undefined' && window.location.protocol !== 'mikoplugin:'

function post(body: unknown) {
  if (window.location.protocol === 'mikoplugin:') {
    try {
      void fetch('__bridge', {
        method: 'POST',
        body: JSON.stringify(body),
      }).catch(() => {})
    } catch {
      // Nothing to report to: the host owns the other end.
    }
    return
  }
  // `bun run dev` has no host: hand the message to the preview host instead.
  if (import.meta.env.DEV) {
    window.dispatchEvent(new CustomEvent(DEV_POST_EVENT, { detail: body }))
  }
}

export function makeSetParamsMessage(
  target: Binding,
  params: readonly { id: string; value: number }[],
) {
  return {
    type: 'futureboard.setParams' as const,
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    ...target,
    params,
  }
}

function flush() {
  scheduled = false
  if (!binding || pending.size === 0) {
    pending.clear()
    return
  }
  const params = Array.from(pending, ([id, value]) => ({ id, value }))
  pending.clear()
  post(makeSetParamsMessage(binding, params))
}

export function postParam(id: keyof MixStationParams, value: number) {
  pending.set(id, value)
  if (scheduled) return
  scheduled = true
  requestAnimationFrame(flush)
}

/**
 * Ask the DAW for a workspace command (e.g. transport play/pause). Independent
 * of the param binding so Space still works before `selectInstance` arrives.
 * When the native shell already claimed the key, this page path does not run.
 */
/**
 * Ask native to validate and, if it approves, bind `instanceId`.
 *
 * Used when the route changed with no approved `selectInstance` behind it — a
 * hash edit or a back/forward. The route is never trusted on its own: native
 * stays the authority on which insert this page is bound to, and answers with a
 * real `selectInstance` (or ignores the request outright).
 */
export function requestSelectInstance(instanceId: string): void {
  if (!instanceId) return
  post({
    type: 'futureboard.requestSelectInstance',
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    instanceId,
  })
}

/** Narrow the optional display metadata, or `null` when it is absent/malformed. */
export function parseDisplay(value: unknown): InstanceDisplay | null {
  if (!value || typeof value !== 'object') return null
  const record = value as Record<string, unknown>
  const fields = ['trackId', 'trackName', 'insertId', 'insertName'] as const
  if (!fields.every((key) => typeof record[key] === 'string')) return null
  return {
    trackId: record.trackId as string,
    trackName: record.trackName as string,
    insertId: record.insertId as string,
    insertName: record.insertName as string,
  }
}

export function postGlobalCommand(commandId: string): void {
  if (!commandId) return
  post({
    type: 'futureboard.globalCommand',
    commandId,
  })
}

export function parseParams(state: unknown): MixStationParams | null {
  if (!state || typeof state !== 'object') return null
  const candidate =
    'params' in state ? (state as { params?: unknown }).params : state
  if (!candidate || typeof candidate !== 'object') return null
  const params = { ...(candidate as Record<string, unknown>) }
  const legacySlots = [
    ['slot1Module', 'filtersEnabled', 1],
    ['slot2Module', 'eqEnabled', 2],
    ['slot3Module', 'compEnabled', 3],
    ['slot4Module', 'satEnabled', 4],
    ['slot5Module', 'widthEnabled', 5],
    ['slot6Module', 'limiterEnabled', 6],
  ] as const
  // Per-module trims postdate the original state shape. A project saved before
  // them has no such field, and strict validation below would otherwise reject
  // the whole state and silently reset the user's chain to defaults.
  const trimKeys = [
    'filtersTrimDb',
    'eqTrimDb',
    'compTrimDb',
    'satTrimDb',
    'widthTrimDb',
    'limiterTrimDb',
  ] as const
  for (const key of trimKeys) {
    if (params[key] === undefined) params[key] = 0
  }

  for (const [slot, enabled, module] of legacySlots) {
    if (params[slot] === undefined) {
      params[slot] = params[enabled] === true ? module : 0
    }
  }

  for (const key of BOOLEAN_KEYS) {
    if (typeof params[key] !== 'boolean') return null
  }
  for (const key of NUMERIC_KEYS) {
    if (typeof params[key] !== 'number' || !Number.isFinite(params[key])) return null
  }
  return params as MixStationParams
}

function isMeterFrame(message: MetersMessage) {
  return (
    Number.isFinite(message.inPeak) &&
    Number.isFinite(message.inRms) &&
    Number.isFinite(message.outPeak) &&
    Number.isFinite(message.outRms) &&
    Number.isFinite(message.gainReductionDb) &&
    typeof message.inClip === 'boolean' &&
    typeof message.outClip === 'boolean'
  )
}

/**
 * Per-stage levels, or an empty array when the field is absent or malformed.
 * An older native build simply omits it, and that must read as "no per-stage
 * telemetry" rather than throwing or drawing zeros as a measurement.
 */
export function stageLevels(value: unknown): readonly number[] {
  if (!Array.isArray(value)) return []
  return value.every((entry) => typeof entry === 'number' && Number.isFinite(entry))
    ? (value as number[])
    : []
}

export function isSpectrumFrame(message: SpectrumMessage) {
  return (
    Array.isArray(message.bins) &&
    message.bins.length > 0 &&
    Number.isFinite(message.minHz) &&
    Number.isFinite(message.maxHz) &&
    Number.isFinite(message.floorDb) &&
    Number.isFinite(message.ceilDb) &&
    message.maxHz > message.minHz &&
    message.ceilDb > message.floorDb
  )
}

export function connectBridge(
  onParams: (params: MixStationParams) => void,
  /**
   * `instanceId` identifies which plug-in instance the page is now bound to.
   * The native shell reuses one browser per plugin id and rebinds it, so this
   * is the only way the editor can tell one instance from another.
   */
  onConnection: (connected: boolean, instance: BoundInstance | null) => void,
  onMeters: (frame: MeterFrame) => void,
  onSpectrum?: (frame: SpectrumFrame) => void,
) {
  post({
    type: 'futureboard.bridgeReady',
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    bridgeVersion: BRIDGE_PROTOCOL_VERSION,
    pluginId: PLUGIN_ID,
  })

  const listener = (event: MessageEvent) => {
    const message = event.data as
      | SelectInstanceMessage
      | InstanceRemovedMessage
      | MetersMessage
      | SpectrumMessage
      | undefined
    if (!message || typeof message !== 'object') return

    if (message.type === 'futureboard.spectrum') {
      if (
        !onSpectrum ||
        message.protocolVersion !== BRIDGE_PROTOCOL_VERSION ||
        binding?.instanceId !== message.instanceId ||
        !isSpectrumFrame(message)
      ) {
        return
      }
      onSpectrum({
        minHz: message.minHz,
        maxHz: message.maxHz,
        floorDb: message.floorDb,
        ceilDb: message.ceilDb,
        bins: Uint8Array.from(message.bins),
      })
      return
    }

    if (message.type === 'futureboard.meters') {
      if (
        binding?.instanceId !== message.instanceId ||
        message.bindingGeneration !== binding.bindingGeneration ||
        !isMeterFrame(message)
      ) {
        return
      }
      onMeters({
        inPeak: message.inPeak,
        inRms: message.inRms,
        outPeak: message.outPeak,
        outRms: message.outRms,
        gainReductionDb: message.gainReductionDb,
        inClip: message.inClip,
        outClip: message.outClip,
        slotInPeak: stageLevels(message.slotInPeak),
        slotOutPeak: stageLevels(message.slotOutPeak),
      })
      return
    }

    if (message.type === 'futureboard.selectInstance') {
      if (
        message.protocolVersion !== BRIDGE_PROTOCOL_VERSION ||
        message.pluginId !== PLUGIN_ID ||
        !Number.isSafeInteger(message.bindingGeneration) ||
        !Number.isSafeInteger(message.stateRevision) ||
        message.bindingGeneration < 0 ||
        message.stateRevision < 0
      ) {
        return
      }
      if (message.bindingGeneration < latestBindingGeneration) return
      if (
        message.bindingGeneration === latestBindingGeneration &&
        latestGenerationRemoved
      ) {
        return
      }
      if (binding) {
        if (
          message.bindingGeneration === binding.bindingGeneration &&
          (message.instanceId !== binding.instanceId ||
            message.stateRevision < stateRevision)
        ) {
          return
        }
      }
      latestBindingGeneration = message.bindingGeneration
      latestGenerationRemoved = false
      binding = {
        pluginId: message.pluginId,
        instanceId: message.instanceId,
        bindingGeneration: message.bindingGeneration,
      }
      stateRevision = message.stateRevision
      pending.clear()
      const params = parseParams(message.state)
      onParams(params ?? defaults)
      onConnection(true, { ...binding, display: parseDisplay(message.display) })
      post({
        type: 'futureboard.instanceReady',
        protocolVersion: BRIDGE_PROTOCOL_VERSION,
        ...binding,
        stateRevision: message.stateRevision,
      })
    } else if (
      message.type === 'futureboard.instanceRemoved' &&
      binding?.instanceId === message.instanceId &&
      message.bindingGeneration === latestBindingGeneration
    ) {
      binding = null
      stateRevision = -1
      latestGenerationRemoved = true
      pending.clear()
      onConnection(false, null)
    }
  }

  window.addEventListener('message', listener)
  return () => {
    window.removeEventListener('message', listener)
    binding = null
    stateRevision = -1
    latestBindingGeneration = -1
    latestGenerationRemoved = false
    pending.clear()
  }
}



