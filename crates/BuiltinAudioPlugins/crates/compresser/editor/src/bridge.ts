/**
 * Native bridge for the Compressor editor.
 *
 * The wire contract every built-in editor speaks: the host posts
 * `futureboard.selectInstance` with the authoritative state, the page answers
 * `futureboard.instanceReady`, and every gesture travels back as a batched
 * `futureboard.setParams` tagged with the binding it was made against. The
 * host drops a batch whose `bindingGeneration` is stale, so an edit made
 * against a torn-down instance can never land on its replacement.
 *
 * Telemetry arrives on the same channel: the input spectrum
 * (`futureboard.spectrum`), the in/out levels and overall reduction
 * (`futureboard.meters`), and each band's reduction
 * (`futureboard.bandReduction`).
 */

import {
  BAND_COUNT,
  CROSSOVER_COUNT,
  MAX_ATTACK_MS,
  MAX_CROSSOVER_HZ,
  MAX_KNEE_DB,
  MAX_MAKEUP_DB,
  MAX_OUTPUT_DB,
  MAX_RATIO,
  MAX_RELEASE_MS,
  MAX_SIDECHAIN_HZ,
  MAX_THRESHOLD_DB,
  MIN_ATTACK_MS,
  MIN_CROSSOVER_HZ,
  MIN_MAKEUP_DB,
  MIN_OUTPUT_DB,
  MIN_RATIO,
  MIN_RELEASE_MS,
  MIN_THRESHOLD_DB,
  SOLO_NONE,
  clamp,
  type Band,
  type CompressorParams,
} from './lib/params'

export const BRIDGE_PROTOCOL_VERSION = 1
export const PLUGIN_ID = 'compresser'

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

export type LevelFrame = {
  inPeak: number
  inRms: number
  outPeak: number
  outRms: number
  /// Decibels being taken off, positive: the single stage's, or the largest
  /// band's in Multi mode.
  gainReductionDb: number
}

type MetersMessage = LevelFrame & {
  type: 'futureboard.meters'
  protocolVersion: number
  instanceId: string
}

type BandReductionMessage = {
  type: 'futureboard.bandReduction'
  protocolVersion: number
  instanceId: string
  reductionDb: number[]
}

export type Telemetry = {
  onSpectrum?: (frame: SpectrumFrame) => void
  onLevels?: (frame: LevelFrame) => void
  onBandReduction?: (reductionDb: number[]) => void
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

const isFiniteNumber = (value: unknown): value is number =>
  typeof value === 'number' && Number.isFinite(value)

const isNumberArray = (value: unknown, length: number): value is number[] =>
  Array.isArray(value) && value.length === length && value.every(isFiniteNumber)

function parseBand(value: unknown): Band | null {
  if (!value || typeof value !== 'object') return null
  const band = value as Record<string, unknown>
  if (
    !isFiniteNumber(band.thresholdDb) ||
    !isFiniteNumber(band.ratio) ||
    !isFiniteNumber(band.attackMs) ||
    !isFiniteNumber(band.releaseMs) ||
    !isFiniteNumber(band.makeupDb) ||
    typeof band.bypass !== 'boolean'
  ) {
    return null
  }
  return {
    thresholdDb: clamp(band.thresholdDb, MIN_THRESHOLD_DB, MAX_THRESHOLD_DB),
    ratio: clamp(band.ratio, MIN_RATIO, MAX_RATIO),
    attackMs: clamp(band.attackMs, MIN_ATTACK_MS, MAX_ATTACK_MS),
    releaseMs: clamp(band.releaseMs, MIN_RELEASE_MS, MAX_RELEASE_MS),
    makeupDb: clamp(band.makeupDb, MIN_MAKEUP_DB, MAX_MAKEUP_DB),
    bypass: band.bypass,
  }
}

/// Accept a host state blob only when every field is present and of the right
/// type. A partial blob is rejected whole rather than merged: a half-applied
/// state would show values the DSP does not have.
export function parseParams(state: unknown): CompressorParams | null {
  if (!state || typeof state !== 'object') return null
  const candidate = 'params' in state ? (state as { params?: unknown }).params : state
  if (!candidate || typeof candidate !== 'object') return null
  const params = candidate as Record<string, unknown>
  if (typeof params.power !== 'boolean') return null
  if (params.mode !== 'single' && params.mode !== 'multi') return null
  const scalars = [
    params.thresholdDb,
    params.ratio,
    params.attackMs,
    params.releaseMs,
    params.makeupDb,
    params.sidechainHpfHz,
    params.kneeDb,
    params.mix,
    params.outputDb,
    params.soloBand,
  ]
  if (!scalars.every(isFiniteNumber)) return null
  if (!isNumberArray(params.crossoverHz, CROSSOVER_COUNT)) return null
  if (!Array.isArray(params.bands) || params.bands.length !== BAND_COUNT) return null
  const bands = params.bands.map(parseBand)
  if (bands.some((band) => band === null)) return null
  const solo = params.soloBand as number
  return {
    power: params.power,
    mode: params.mode,
    thresholdDb: clamp(params.thresholdDb as number, MIN_THRESHOLD_DB, MAX_THRESHOLD_DB),
    ratio: clamp(params.ratio as number, MIN_RATIO, MAX_RATIO),
    attackMs: clamp(params.attackMs as number, MIN_ATTACK_MS, MAX_ATTACK_MS),
    releaseMs: clamp(params.releaseMs as number, MIN_RELEASE_MS, MAX_RELEASE_MS),
    makeupDb: clamp(params.makeupDb as number, MIN_MAKEUP_DB, MAX_MAKEUP_DB),
    sidechainHpfHz: clamp(params.sidechainHpfHz as number, 0, MAX_SIDECHAIN_HZ),
    kneeDb: clamp(params.kneeDb as number, 0, MAX_KNEE_DB),
    mix: clamp(params.mix as number, 0, 100),
    outputDb: clamp(params.outputDb as number, MIN_OUTPUT_DB, MAX_OUTPUT_DB),
    crossoverHz: params.crossoverHz.map((hz) => clamp(hz, MIN_CROSSOVER_HZ, MAX_CROSSOVER_HZ)),
    bands: bands as Band[],
    soloBand: Number.isInteger(solo) && solo >= 0 && solo < BAND_COUNT ? solo : SOLO_NONE,
  }
}

export function connectBridge(
  onParams: (params: CompressorParams) => void,
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
      | MetersMessage
      | BandReductionMessage
      | undefined
    if (!message || typeof message !== 'object') return

    // The ~30 Hz telemetry first, so it never reaches the binding bookkeeping.
    if (message.type === 'futureboard.meters') {
      if (binding?.instanceId !== message.instanceId) return
      telemetry.onLevels?.({
        inPeak: message.inPeak,
        inRms: message.inRms,
        outPeak: message.outPeak,
        outRms: message.outRms,
        gainReductionDb: isFiniteNumber(message.gainReductionDb) ? message.gainReductionDb : 0,
      })
      return
    }
    if (message.type === 'futureboard.bandReduction') {
      if (binding?.instanceId !== message.instanceId) return
      if (!isNumberArray(message.reductionDb, BAND_COUNT)) return
      telemetry.onBandReduction?.(message.reductionDb)
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
