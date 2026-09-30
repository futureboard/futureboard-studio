/**
 * Native bridge — the wire contract every built-in editor speaks.
 *
 * The host posts `futureboard.selectInstance` with the authoritative state,
 * the page answers `futureboard.instanceReady`, and every gesture travels back
 * as a batched `futureboard.setParams` tagged with the binding it was made
 * against. The host drops a batch whose `bindingGeneration` is stale, so an
 * edit made against a torn-down instance can never land on its replacement.
 * Telemetry arrives on the same channel as `futureboard.meters`.
 *
 * Under `bun run dev` there is no host: outgoing messages are handed to the
 * dev-only preview host (`src/dev/previewHost.ts`) instead, which answers
 * through the same `message` listener native uses.
 */

export const BRIDGE_PROTOCOL_VERSION = 1

/// One telemetry frame measured by the DSP on this insert. Levels are linear.
export type MeterFrame = {
  inPeak: number
  inRms: number
  outPeak: number
  outRms: number
  /// Decibels taken off (or, for a shaper, moved), positive.
  gainReductionDb: number
  inClip: boolean
  outClip: boolean
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

type MetersMessage = MeterFrame & {
  type: 'futureboard.meters'
  protocolVersion: number
  instanceId: string
}

/// Event the dev preview host listens for. Never dispatched in a production
/// bundle: `import.meta.env.DEV` is compiled to `false` there.
export const DEV_POST_EVENT = 'futureboard:dev-post'

let binding: Binding | null = null
const pending = new Map<string, number>()
let scheduled = false

function post(body: unknown) {
  if (window.location.protocol === 'mikoplugin:') {
    try {
      void fetch('__bridge', { method: 'POST', body: JSON.stringify(body) }).catch(() => {})
    } catch {
      // Nothing to report to: the host owns the other end.
    }
    return
  }
  if (import.meta.env.DEV) {
    window.dispatchEvent(new CustomEvent(DEV_POST_EVENT, { detail: body }))
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

const finite = (value: unknown): value is number => typeof value === 'number' && Number.isFinite(value)

function meterFrame(message: MetersMessage): MeterFrame | null {
  if (
    !finite(message.inPeak) ||
    !finite(message.inRms) ||
    !finite(message.outPeak) ||
    !finite(message.outRms) ||
    !finite(message.gainReductionDb)
  ) {
    return null
  }
  return {
    inPeak: message.inPeak,
    inRms: message.inRms,
    outPeak: message.outPeak,
    outRms: message.outRms,
    gainReductionDb: message.gainReductionDb,
    inClip: message.inClip === true,
    outClip: message.outClip === true,
  }
}

export function connectBridge<P>({
  pluginId,
  parse,
  onParams,
  onConnection,
  onMeters,
}: {
  pluginId: string
  /// Accept a host state blob, or `null` to keep what the page shows.
  parse: (state: unknown) => P | null
  onParams: (params: P) => void
  onConnection: (connected: boolean) => void
  onMeters: (frame: MeterFrame) => void
}) {
  post({
    type: 'futureboard.bridgeReady',
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    bridgeVersion: BRIDGE_PROTOCOL_VERSION,
    pluginId,
  })

  const listener = (event: MessageEvent) => {
    const message = event.data as SelectInstanceMessage | InstanceRemovedMessage | MetersMessage | undefined
    if (!message || typeof message !== 'object') return

    // The ~30 Hz telemetry first, so it never reaches the binding bookkeeping.
    if (message.type === 'futureboard.meters') {
      if (binding?.instanceId !== message.instanceId) return
      const frame = meterFrame(message)
      if (frame) onMeters(frame)
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
      const params = parse(message.state)
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
    } else if (message.type === 'futureboard.instanceRemoved' && binding?.instanceId === message.instanceId) {
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

/// Whether this page is the dev-server preview rather than an embedded editor.
export const IS_BROWSER_PREVIEW = import.meta.env.DEV && window.location.protocol !== 'mikoplugin:'
