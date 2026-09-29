/**
 * Native bridge for the Imager editor.
 *
 * The wire contract every built-in editor speaks: the host posts
 * `futureboard.selectInstance` with the authoritative state, the page answers
 * `futureboard.instanceReady`, and every gesture travels back as a batched
 * `futureboard.setParams` tagged with the binding it was made against. The
 * host drops a batch whose `bindingGeneration` is stale, so an edit made
 * against a torn-down instance can never land on its replacement.
 *
 * Telemetry arrives on the same channel: the input spectrum
 * (`futureboard.spectrum`), the stereo image the DSP measures on its own
 * output (`futureboard.stereoImage`), and the in/out levels
 * (`futureboard.meters`).
 */

import {
  BAND_COUNT,
  CROSSOVER_COUNT,
  MAX_OUTPUT_DB,
  MAX_WIDTH,
  MIN_OUTPUT_DB,
  SOLO_NONE,
  clamp,
  type ImagerParams,
} from './lib/params'

export const BRIDGE_PROTOCOL_VERSION = 1
export const PLUGIN_ID = 'imager'

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

/// One analyser frame measured on the audio *arriving* at this insert. `bins`
/// are log-spaced across `minHz..maxHz` and quantised to bytes: `0` is
/// `floorDb`, `255` is `ceilDb`.
export type SpectrumFrame = {
  minHz: number
  maxHz: number
  floorDb: number
  ceilDb: number
  bins: number[]
}

type SpectrumMessage = SpectrumFrame & {
  type: 'futureboard.spectrum'
  protocolVersion: number
  instanceId: string
}

/// The stereo image of the plugin's *output*, measured by the DSP.
export type StereoImageFrame = {
  /// −1 (one side inverted) .. +1 (mono); 0 while too quiet to measure.
  correlation: number
  bandCorrelation: number[]
  /// Linear RMS per band, so an empty band's 0 correlation can be told apart.
  bandLevel: number[]
  /// Interleaved left/right pairs, oldest first, as signed bytes (±127 is
  /// full scale).
  scope: number[]
}

type StereoImageMessage = StereoImageFrame & {
  type: 'futureboard.stereoImage'
  protocolVersion: number
  instanceId: string
}

export type LevelFrame = {
  inPeak: number
  inRms: number
  outPeak: number
  outRms: number
}

type MetersMessage = LevelFrame & {
  type: 'futureboard.meters'
  protocolVersion: number
  instanceId: string
}

export type Telemetry = {
  onSpectrum?: (frame: SpectrumFrame) => void
  onStereoImage?: (frame: StereoImageFrame) => void
  onLevels?: (frame: LevelFrame) => void
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
    // A standalone design preview intentionally has no native endpoint.
  }
}

/// Coalesce a frame's worth of edits into one batch. A drag emits on every
/// pointer move; the map keeps the *last* value per id, so the committed value
/// is always sent even though the intermediate ones are not.
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

export function postParam(id: string, value: number) {
  pending.set(id, value)
  if (scheduled) return
  scheduled = true
  requestAnimationFrame(flush)
}

const isNumberArray = (value: unknown, length: number): value is number[] =>
  Array.isArray(value) &&
  value.length === length &&
  value.every((entry) => typeof entry === 'number' && Number.isFinite(entry))

/// Accept a host state blob only when every field is present and of the right
/// type. A partial blob is rejected whole rather than merged: a half-applied
/// state would show values the DSP does not have.
export function parseParams(state: unknown): ImagerParams | null {
  if (!state || typeof state !== 'object') return null
  const candidate = 'params' in state ? (state as { params?: unknown }).params : state
  if (!candidate || typeof candidate !== 'object') return null
  const params = candidate as Record<string, unknown>
  if (typeof params.power !== 'boolean') return null
  if (!isNumberArray(params.crossoverHz, CROSSOVER_COUNT)) return null
  if (!isNumberArray(params.width, BAND_COUNT)) return null
  if (typeof params.soloBand !== 'number' || typeof params.outputDb !== 'number') return null
  return {
    power: params.power,
    crossoverHz: [...params.crossoverHz],
    width: params.width.map((width) => clamp(width, 0, MAX_WIDTH)),
    soloBand:
      Number.isInteger(params.soloBand) && params.soloBand >= 0 && params.soloBand < BAND_COUNT
        ? params.soloBand
        : SOLO_NONE,
    outputDb: clamp(params.outputDb, MIN_OUTPUT_DB, MAX_OUTPUT_DB),
  }
}

export function connectBridge(
  onParams: (params: ImagerParams) => void,
  onConnection: (connected: boolean) => void,
  telemetry: Telemetry = {},
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
      | SpectrumMessage
      | StereoImageMessage
      | MetersMessage
      | undefined
    if (!message || typeof message !== 'object') return

    // The ~30 Hz telemetry first, so it never reaches the binding bookkeeping.
    if (message.type === 'futureboard.stereoImage') {
      if (binding?.instanceId !== message.instanceId) return
      if (!Array.isArray(message.scope) || !Array.isArray(message.bandCorrelation)) return
      telemetry.onStereoImage?.({
        correlation: message.correlation,
        bandCorrelation: message.bandCorrelation,
        bandLevel: Array.isArray(message.bandLevel) ? message.bandLevel : [],
        scope: message.scope,
      })
      return
    }
    if (message.type === 'futureboard.spectrum') {
      if (binding?.instanceId !== message.instanceId || !Array.isArray(message.bins)) return
      telemetry.onSpectrum?.({
        minHz: message.minHz,
        maxHz: message.maxHz,
        floorDb: message.floorDb,
        ceilDb: message.ceilDb,
        bins: message.bins,
      })
      return
    }
    if (message.type === 'futureboard.meters') {
      if (binding?.instanceId !== message.instanceId) return
      telemetry.onLevels?.({
        inPeak: message.inPeak,
        inRms: message.inRms,
        outPeak: message.outPeak,
        outRms: message.outRms,
      })
      return
    }

    if (message.type === 'futureboard.selectInstance') {
      binding = {
        pluginId: message.pluginId,
        instanceId: message.instanceId,
        bindingGeneration: message.bindingGeneration,
      }
      // Edits queued against the previous binding are abandoned, not
      // re-tagged: they were made against different state.
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
    }
  }

  window.addEventListener('message', listener)
  return () => {
    window.removeEventListener('message', listener)
    binding = null
    pending.clear()
  }
}
