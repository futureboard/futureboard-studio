export const BRIDGE_PROTOCOL_VERSION = 1
export const PLUGIN_ID = 'drumsampler'
export const PAD_COUNT = 16

export type Pad = {
  note: number
  tune: number
  gain: number
  pan: number
  choke: number
  attack: number
  release: number
  reverse: boolean
  mute: boolean
  solo: boolean
  sampleName: string | null
}

export type DrumSamplerParams = {
  pads: Pad[]
}

/// Wire id for one field of one pad, e.g. `pad3Gain` — must match the flat
/// `UI_PARAM_IDS` table generated in `drumsampler::ipc` (Rust), field order:
/// note, tune, gain, pan, choke, attack, release, reverse, mute, solo.
const FIELD_SUFFIX = {
  note: 'Note',
  tune: 'Tune',
  gain: 'Gain',
  pan: 'Pan',
  choke: 'Choke',
  attack: 'Attack',
  release: 'Release',
  reverse: 'Reverse',
  mute: 'Mute',
  solo: 'Solo',
} as const

export type WireField = keyof typeof FIELD_SUFFIX

export function padParamId(padIndex: number, field: WireField): string {
  return `pad${padIndex}${FIELD_SUFFIX[field]}`
}

type Binding = {
  pluginId: string
  instanceId: string
  bindingGeneration: number
}

type SelectInstanceMessage = {
  type: 'futureboard.selectInstance'
  protocolVersion: number
  pluginId: string
  instanceId: string
  bindingGeneration: number
  stateRevision: number
  state: unknown
}

type InstanceRemovedMessage = {
  type: 'futureboard.instanceRemoved'
  protocolVersion: number
  instanceId: string
}

export type SampleLoadResult = {
  padIndex: number
  ok: boolean
  name: string
  error: string | null
}

type DrumSampleLoadResultMessage = {
  type: 'futureboard.drumSampleLoadResult'
  protocolVersion: number
  instanceId: string
  padIndex: number
  ok: boolean
  name: string
  error: string | null
  frames: number
  channels: number
}

let binding: Binding | null = null
const pending = new Map<string, number>()
let scheduled = false

function post(body: unknown) {
  if (window.location.protocol !== 'mikoplugin:') return
  try {
    void fetch('__bridge', {
      method: 'POST',
      body: JSON.stringify(body),
    }).catch(() => {})
  } catch {
    // Standalone Vite preview intentionally has no native bridge.
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
  post({
    type: 'futureboard.setParams',
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    ...binding,
    params,
  })
}

/** Batches live knob/toggle edits into one `setParams` per animation frame. */
export function postParam(id: string, value: number) {
  pending.set(id, value)
  if (scheduled) return
  scheduled = true
  requestAnimationFrame(flush)
}

/** Load an already-sandboxed file (already listed / just imported) onto a pad. */
export function postLoadSample(padIndex: number, fileName: string) {
  if (!binding) return
  post({
    type: 'futureboard.loadSample',
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    ...binding,
    padIndex,
    fileName,
  })
}

/** Ask native to open the OS file picker and import a sample onto a pad. */
export function postBrowseSample(padIndex: number) {
  if (!binding) return
  post({
    type: 'futureboard.browseSample',
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    ...binding,
    padIndex,
  })
}

function defaultPad(index: number): Pad {
  return {
    note: 36 + index,
    tune: 0,
    gain: 0,
    pan: 0,
    choke: 0,
    attack: 1,
    release: 60,
    reverse: false,
    mute: false,
    solo: false,
    sampleName: null,
  }
}

export function defaultParams(): DrumSamplerParams {
  return { pads: Array.from({ length: PAD_COUNT }, (_, index) => defaultPad(index)) }
}

function parseParams(state: unknown): DrumSamplerParams | null {
  if (!state || typeof state !== 'object') return null
  const candidate = 'params' in state ? (state as { params?: unknown }).params : state
  if (!candidate || typeof candidate !== 'object' || !('pads' in candidate)) return null
  const rawPads = (candidate as { pads?: unknown }).pads
  if (!Array.isArray(rawPads)) return null
  const pads = rawPads.map((raw, index) => {
    const source = (raw ?? {}) as Partial<Record<string, unknown>>
    const fallback = defaultPad(index)
    return {
      note: typeof source.note === 'number' ? source.note : fallback.note,
      tune: typeof source.tuneSemitones === 'number' ? source.tuneSemitones : fallback.tune,
      gain: typeof source.gainDb === 'number' ? source.gainDb : fallback.gain,
      pan: typeof source.pan === 'number' ? source.pan : fallback.pan,
      choke: typeof source.chokeGroup === 'number' ? source.chokeGroup : fallback.choke,
      attack: typeof source.attackMs === 'number' ? source.attackMs : fallback.attack,
      release: typeof source.releaseMs === 'number' ? source.releaseMs : fallback.release,
      reverse: typeof source.reverse === 'boolean' ? source.reverse : fallback.reverse,
      mute: typeof source.muted === 'boolean' ? source.muted : fallback.mute,
      solo: typeof source.solo === 'boolean' ? source.solo : fallback.solo,
      sampleName: typeof source.sampleName === 'string' ? source.sampleName : null,
    }
  })
  if (pads.length !== PAD_COUNT) return null
  return { pads }
}

export function connectBridge(
  onParams: (params: DrumSamplerParams) => void,
  onConnection: (connected: boolean) => void,
  onSampleResult: (result: SampleLoadResult) => void,
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
      | DrumSampleLoadResultMessage
      | undefined
    if (!message || typeof message !== 'object') return
    if (message.type === 'futureboard.selectInstance') {
      binding = {
        pluginId: message.pluginId,
        instanceId: message.instanceId,
        bindingGeneration: message.bindingGeneration,
      }
      pending.clear()
      const params = parseParams(message.state)
      if (params) onParams(params)
      onConnection(true)
      post({
        type: 'futureboard.instanceReady',
        protocolVersion: BRIDGE_PROTOCOL_VERSION,
        pluginId: message.pluginId,
        instanceId: message.instanceId,
        bindingGeneration: message.bindingGeneration,
        stateRevision: message.stateRevision,
      })
    } else if (
      message.type === 'futureboard.instanceRemoved' &&
      binding?.instanceId === message.instanceId
    ) {
      binding = null
      pending.clear()
      onConnection(false)
    } else if (
      message.type === 'futureboard.drumSampleLoadResult' &&
      binding?.instanceId === message.instanceId
    ) {
      onSampleResult({
        padIndex: message.padIndex,
        ok: message.ok,
        name: message.name,
        error: message.error,
      })
    }
  }

  window.addEventListener('message', listener)
  return () => {
    window.removeEventListener('message', listener)
    binding = null
    pending.clear()
  }
}
