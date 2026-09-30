/**
 * Dev-only stand-in for the Futureboard host, for debugging the editor in a
 * browser (`bun run dev`).
 *
 * It answers the page through the same `message` events native uses —
 * `selectInstance`, then `meters` (with per-stage levels) and `spectrum` at
 * ~30 Hz — and listens for the page's outgoing messages on `DEV_POST_EVENT` so
 * its controls move the simulation.
 *
 * Everything it sends comes from a synthetic groove run through a rough model
 * of the rack, not from the DSP. The header says so while it runs, and none of
 * this reaches the embedded editor: `main.tsx` imports it only when
 * `import.meta.env.DEV` is true, which the production build compiles to
 * `false` and drops.
 */

import { BRIDGE_PROTOCOL_VERSION, DEV_POST_EVENT, PLUGIN_ID, defaults } from '../bridge'
import { grooveFrame } from './signal'
import { createRackModel, spectrumFrame } from './simulation'

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
  const wire: Record<string, number> = Object.fromEntries(
    Object.entries(defaults).map(([id, value]) => [id, typeof value === 'boolean' ? (value ? 1 : 0) : value]),
  )
  const rack = createRackModel()
  let generation = 0
  let time = 0

  const send = (message: object) => window.postMessage(message, '*')
  const select = () => {
    generation += 1
    send({
      type: 'futureboard.selectInstance',
      protocolVersion: BRIDGE_PROTOCOL_VERSION,
      pluginId: PLUGIN_ID,
      instanceId: INSTANCE_ID,
      bindingGeneration: generation,
      stateRevision: 0,
      display: { trackId: 'preview', trackName: 'Preview Track', insertId: INSTANCE_ID, insertName: 'MixStation' },
      // No blob: the page falls back to its defaults, which match `wire`.
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
    const frame = rack(wire, input, FRAME_SECONDS)
    send({
      type: 'futureboard.meters',
      protocolVersion: BRIDGE_PROTOCOL_VERSION,
      instanceId: INSTANCE_ID,
      bindingGeneration: generation,
      ...frame,
    })
    send({
      type: 'futureboard.spectrum',
      protocolVersion: BRIDGE_PROTOCOL_VERSION,
      instanceId: INSTANCE_ID,
      ...spectrumFrame(time, input.onset),
    })
  }, FRAME_SECONDS * 1000)
}
