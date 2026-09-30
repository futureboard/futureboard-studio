/**
 * Dev-only stand-in for the Futureboard host, for debugging the editor in a
 * browser (`bun run dev`).
 *
 * It answers the page through the same `message` events native uses —
 * `selectInstance`, then `meters` at ~30 Hz — and listens for the page's
 * outgoing messages on `DEV_POST_EVENT` so its knobs move the simulation.
 *
 * The meters come from a synthetic drum groove run through a rough model of
 * the plugin (`simulation.ts`), not from the DSP. The header says so while it
 * runs, and none of this reaches the embedded editor: `main.tsx` imports it
 * only when `import.meta.env.DEV` is true, which the production build compiles
 * to `false` and drops.
 */

import { BRIDGE_PROTOCOL_VERSION, DEV_POST_EVENT } from '../bridge'
import { PREVIEW_DEFAULTS, PREVIEW_PLUGIN_ID, createPreviewProcessor } from './simulation'
import { grooveFrame } from './signal'

const FRAME_SECONDS = 1 / 30
const INSTANCE_ID = 'browser-preview'

type OutgoingMessage = {
  type?: string
  params?: { id: string; value: number }[]
}

let started = false

export function startPreviewHost() {
  if (started) return
  started = true
  const wire: Record<string, number> = { ...PREVIEW_DEFAULTS }
  const process = createPreviewProcessor()
  let generation = 0
  let time = 0

  const send = (message: object) => window.postMessage(message, '*')
  const select = () => {
    generation += 1
    send({
      type: 'futureboard.selectInstance',
      protocolVersion: BRIDGE_PROTOCOL_VERSION,
      pluginId: PREVIEW_PLUGIN_ID,
      instanceId: INSTANCE_ID,
      bindingGeneration: generation,
      stateRevision: 0,
      // No blob: the page keeps its own defaults, which match `wire`.
      state: null,
    })
  }

  window.addEventListener(DEV_POST_EVENT, (event) => {
    const message = (event as CustomEvent<OutgoingMessage>).detail
    if (message?.type === 'futureboard.bridgeReady') select()
    if (message?.type === 'futureboard.setParams') {
      for (const { id, value } of message.params ?? []) wire[id] = value
    }
  })

  select()
  window.setInterval(() => {
    time += FRAME_SECONDS
    const input = grooveFrame(time, FRAME_SECONDS)
    const output = process(wire, input, FRAME_SECONDS)
    const bypassed = (wire.power ?? 1) < 0.5
    send({
      type: 'futureboard.meters',
      protocolVersion: BRIDGE_PROTOCOL_VERSION,
      instanceId: INSTANCE_ID,
      bindingGeneration: generation,
      inPeak: input.peak,
      inRms: input.rms,
      outPeak: bypassed ? input.peak : output.peak,
      outRms: bypassed ? input.rms : output.rms,
      gainReductionDb: bypassed ? 0 : output.reductionDb,
      inClip: input.peak >= 1,
      outClip: !bypassed && output.peak >= 1,
    })
  }, FRAME_SECONDS * 1000)
}
