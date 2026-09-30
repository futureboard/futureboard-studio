/**
 * Native bridge for the Drum Sampler editor.
 *
 * The wire contract every built-in editor speaks: the host posts
 * `futureboard.selectInstance` with the authoritative state, the page answers
 * `futureboard.instanceReady`, and every gesture travels back as a batched
 * `futureboard.setParams` tagged with the binding it was made against. A
 * pad's audio is not a parameter — it is loaded by file name from the
 * plugin's Samples folder (`loadSample` / `browseSample`), or dropped from the
 * OS onto the window, where native takes the files and the page only says
 * which pad (`fileDrop` → `placeDroppedFiles`). Either way it comes back as
 * `futureboard.drumSampleLoadResult` with the waveform the host decoded.
 */

import { parseKit, type Kit } from './lib/pads'

export const BRIDGE_PROTOCOL_VERSION = 1
export const PLUGIN_ID = 'drumsampler'

type Binding = {
  pluginId: string
  instanceId: string
  bindingGeneration: number
}

/// One pad's sample as the host decoded it.
export type SampleInfo = {
  name: string
  frames: number
  channels: number
  sampleRate: number
  /// Loudest absolute sample per slice, `0..255`.
  peaks: number[]
}

export type SampleLoadResult = {
  padIndex: number
  ok: boolean
  name: string
  error: string | null
  sample: SampleInfo | null
}

/// OS files over the page (`futureboard.fileDrag`), in page pixels.
export type FileDrag = { x: number; y: number; count: number }

/// OS files dropped on the page (`futureboard.fileDrop`), held by native.
export type FileDrop = { dropId: number; x: number; y: number; fileNames: string[]; rejected: number }

export type SampleFile = {
  fileName: string
  sizeBytes: number
  modifiedMs: number
}

export type Handlers = {
  onKit: (kit: Kit) => void
  onConnection: (connected: boolean) => void
  onSampleResult: (result: SampleLoadResult) => void
  onPadLevels?: (levels: number[]) => void
  onOutput?: (peak: number, rms: number) => void
  onFiles?: (files: SampleFile[]) => void
  onFileDrag?: (drag: FileDrag | null) => void
  onFileDrop?: (drop: FileDrop) => void
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

/** Batches live edits into one `setParams` per animation frame. */
export function postParam(id: string, value: number) {
  pending.set(id, value)
  if (scheduled) return
  scheduled = true
  requestAnimationFrame(flush)
}

/** Load a file already in the Samples folder onto a pad. */
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

/**
 * Say where the files of a `futureboard.fileDrop` go: onto consecutive pads
 * from `padIndex`, or only into the Samples folder (`null`). Native holds the
 * files, reads them and sends them to the plug-in host itself — the page only
 * ever sees their names.
 */
export function postPlaceDroppedFiles(dropId: number, padIndex: number | null) {
  if (!binding) return
  post({
    type: 'futureboard.placeDroppedFiles',
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    ...binding,
    dropId,
    padIndex,
  })
}

/** Ask for the Samples folder listing (answered with `futureboard.fileList`). */
export function postListSamples() {
  post({
    type: 'futureboard.listFiles',
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    pluginId: PLUGIN_ID,
    kind: 'samples',
  })
}

type Message = { type?: string; instanceId?: string } & Record<string, unknown>

export function connectBridge(handlers: Handlers) {
  post({
    type: 'futureboard.bridgeReady',
    protocolVersion: BRIDGE_PROTOCOL_VERSION,
    bridgeVersion: BRIDGE_PROTOCOL_VERSION,
    pluginId: PLUGIN_ID,
  })

  const listener = (event: MessageEvent) => {
    const message = event.data as Message | undefined
    if (!message || typeof message !== 'object') return
    const mine = binding !== null && message.instanceId === binding.instanceId

    switch (message.type) {
      case 'futureboard.padLevels':
        if (mine && Array.isArray(message.levels)) handlers.onPadLevels?.(message.levels as number[])
        return
      case 'futureboard.meters':
        if (mine) handlers.onOutput?.(Number(message.outPeak) || 0, Number(message.outRms) || 0)
        return
      case 'futureboard.fileDrag':
        handlers.onFileDrag?.(
          message.active === true
            ? { x: Number(message.x) || 0, y: Number(message.y) || 0, count: Number(message.count) || 0 }
            : null,
        )
        return
      case 'futureboard.fileDrop':
        handlers.onFileDrop?.({
          dropId: Number(message.dropId),
          x: Number(message.x) || 0,
          y: Number(message.y) || 0,
          fileNames: Array.isArray(message.fileNames) ? (message.fileNames as unknown[]).map(String) : [],
          rejected: Number(message.rejected) || 0,
        })
        return
      case 'futureboard.fileList':
        if (message.kind === 'samples' && Array.isArray(message.files)) {
          handlers.onFiles?.(message.files as SampleFile[])
        }
        return
      case 'futureboard.drumSampleLoadResult': {
        if (!mine) return
        const ok = message.ok === true
        const peaks = Array.isArray(message.peaks) ? (message.peaks as number[]) : []
        handlers.onSampleResult({
          padIndex: Number(message.padIndex),
          ok,
          name: String(message.name ?? ''),
          error: typeof message.error === 'string' ? message.error : null,
          sample: ok
            ? {
                name: String(message.name ?? ''),
                frames: Number(message.frames) || 0,
                channels: Number(message.channels) || 0,
                sampleRate: Number(message.sampleRate) || 0,
                peaks,
              }
            : null,
        })
        return
      }
      case 'futureboard.selectInstance': {
        binding = {
          pluginId: String(message.pluginId),
          instanceId: String(message.instanceId),
          bindingGeneration: Number(message.bindingGeneration),
        }
        // Edits queued against the previous binding are abandoned, not
        // re-tagged: they were made against different state.
        pending.clear()
        const kit = parseKit(message.state)
        if (kit) handlers.onKit(kit)
        handlers.onConnection(true)
        post({
          type: 'futureboard.instanceReady',
          protocolVersion: BRIDGE_PROTOCOL_VERSION,
          pluginId: message.pluginId,
          instanceId: message.instanceId,
          bindingGeneration: message.bindingGeneration,
          stateRevision: message.stateRevision,
        })
        return
      }
      case 'futureboard.instanceRemoved':
        if (!mine) return
        binding = null
        pending.clear()
        handlers.onConnection(false)
        return
    }
  }

  window.addEventListener('message', listener)
  return () => {
    window.removeEventListener('message', listener)
    binding = null
    pending.clear()
  }
}
