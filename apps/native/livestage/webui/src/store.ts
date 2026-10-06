// The page's one connection to the server and everything it has been told.
//
// Session, status and the rest are React state through `useStore`. Meters
// are not: they arrive thirty times a second, so they go into a plain map
// that the meter canvases read on their own animation frame.

import { useSyncExternalStore } from 'react'
import type {
  BusStrip,
  ChannelStrip,
  Command,
  Hello,
  InsertState,
  InstalledEffect,
  ServerMessage,
  Session,
  StatusMessage,
  TelemetryFrame,
} from './protocol.ts'
import { stripKey } from './protocol.ts'

export interface Notice {
  text: string
  error: boolean
  at: number
}

export interface State {
  connection: 'connecting' | 'open' | 'closed'
  hello: Hello | null
  session: Session | null
  status: StatusMessage | null
  /** Inserts that are loading or failed, by id; replaced only on change. */
  insertStates: Record<string, InsertState>
  installed: InstalledEffect[] | null
  notice: Notice | null
}

type Reply = { ok: boolean; error?: string } & Record<string, unknown>

let state: State = {
  connection: 'connecting',
  hello: null,
  session: null,
  status: null,
  insertStates: {},
  installed: null,
  notice: null,
}
const listeners = new Set<() => void>()

function setState(patch: Partial<State>) {
  state = { ...state, ...patch }
  for (const listener of listeners) listener()
}

function subscribe(listener: () => void) {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

export function getState(): State {
  return state
}

/** `selector` must return something already in the state (or a primitive). */
export function useStore<T>(selector: (state: State) => T): T {
  return useSyncExternalStore(subscribe, () => selector(state))
}

export function notify(text: string, error = false) {
  setState({ notice: { text, error, at: Date.now() } })
}

// ── Meters ──────────────────────────────────────────────────────────────

export interface MeterState {
  output: [number, number]
  input: [number, number]
  /** When the output or input last reached full scale (ms); -Infinity if never. */
  outputClip: number
  inputClip: number
  /** The peak line: the highest recent level and when it was reached. */
  outputHold: [number, number]
  inputHold: [number, number]
  outputHoldAt: [number, number]
  inputHoldAt: [number, number]
}

export const meters = new Map<string, MeterState>()

function takeMeters(update: Extract<ServerMessage, { type: 'meters' }>) {
  const now = performance.now()
  for (const { strip, levels } of update.meters) {
    const key = stripKey(strip)
    let meter = meters.get(key)
    if (!meter) {
      meter = {
        output: [0, 0],
        input: [0, 0],
        outputClip: -Infinity,
        inputClip: -Infinity,
        outputHold: [0, 0],
        inputHold: [0, 0],
        outputHoldAt: [0, 0],
        inputHoldAt: [0, 0],
      }
      meters.set(key, meter)
    }
    for (const side of [0, 1] as const) {
      meter.output[side] = Math.max(meter.output[side], levels.output[side])
      meter.input[side] = Math.max(meter.input[side], levels.input[side])
      if (levels.output[side] >= meter.outputHold[side]) {
        meter.outputHold[side] = levels.output[side]
        meter.outputHoldAt[side] = now
      }
      if (levels.input[side] >= meter.inputHold[side]) {
        meter.inputHold[side] = levels.input[side]
        meter.inputHoldAt[side] = now
      }
    }
    if (Math.max(...levels.output) >= 1) meter.outputClip = now
    if (Math.max(...levels.input) >= 1) meter.inputClip = now
  }
}

// ── Session updates ─────────────────────────────────────────────────────

/** Keep the previous object for every strip that did not change, so a
 *  memoised strip only redraws when it is the one that moved. */
function share(previous: Session | null, next: Session): Session {
  if (!previous) return next
  const reuse = <T extends ChannelStrip | BusStrip>(old: T[], fresh: T[]): T[] =>
    fresh.map((strip) => {
      const before = old.find((o) => o.id === strip.id)
      return before && JSON.stringify(before) === JSON.stringify(strip) ? before : strip
    })
  return {
    ...next,
    channels: reuse(previous.channels, next.channels),
    buses: reuse(previous.buses, next.buses),
    master:
      JSON.stringify(previous.master) === JSON.stringify(next.master) ? previous.master : next.master,
    outputs:
      JSON.stringify(previous.outputs) === JSON.stringify(next.outputs)
        ? previous.outputs
        : next.outputs,
  }
}

function receive(message: ServerMessage) {
  switch (message.type) {
    case 'hello':
      setState({ hello: message })
      break
    case 'session':
      setState({ session: share(state.session, message.session) })
      break
    case 'status': {
      const patch: Partial<State> = { status: message }
      if (JSON.stringify(message.inserts) !== JSON.stringify(state.insertStates)) {
        patch.insertStates = message.inserts
      }
      setState(patch)
      break
    }
    case 'meters':
      takeMeters(message)
      break
    case 'telemetry':
      for (const handler of telemetryHandlers) handler(message.insert, message.frame)
      break
    case 'reply': {
      if (typeof message.id === 'number') {
        const resolve = pending.get(message.id)
        pending.delete(message.id)
        resolve?.(message)
      }
      break
    }
  }
}

// ── Plug-in editor telemetry ────────────────────────────────────────────
// Handled by editors/live.ts; kept apart so this module does not import it.

type TelemetryHandler = (insert: number, frame: TelemetryFrame) => void
const telemetryHandlers = new Set<TelemetryHandler>()
const openHandlers = new Set<() => void>()

export function onTelemetry(handler: TelemetryHandler) {
  telemetryHandlers.add(handler)
}

/** Called each time the socket (re)connects: watches must be re-sent. */
export function onOpen(handler: () => void) {
  openHandlers.add(handler)
}

// ── The socket ──────────────────────────────────────────────────────────

let socket: WebSocket | null = null
let nextId = 1
const pending = new Map<number, (reply: Reply) => void>()

export function connect() {
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws'
  const ws = new WebSocket(`${scheme}://${location.host}/ws`)
  socket = ws
  setState({ connection: 'connecting' })
  ws.onopen = () => {
    setState({ connection: 'open' })
    for (const handler of openHandlers) handler()
  }
  ws.onmessage = (event) => {
    try {
      receive(JSON.parse(event.data as string) as ServerMessage)
    } catch (error) {
      console.error('LiveStage: bad message', error)
    }
  }
  ws.onclose = () => {
    if (socket !== ws) return
    socket = null
    for (const resolve of pending.values()) resolve({ ok: false, error: 'disconnected' })
    pending.clear()
    setState({ connection: 'closed' })
    // The server may be restarting; keep trying.
    window.setTimeout(connect, 1000)
  }
}

/** Send a command and wait for the server's answer. */
export function request(command: Command): Promise<Reply> {
  if (!socket || socket.readyState !== WebSocket.OPEN) {
    return Promise.resolve({ ok: false, error: 'not connected' })
  }
  const id = nextId++
  socket.send(JSON.stringify({ ...command, id }))
  return new Promise((resolve) => pending.set(id, resolve))
}

/** Send a command; say so if the server refused it. */
export function act(command: Command) {
  void request(command).then((reply) => {
    if (!reply.ok) notify(reply.error ?? 'refused', true)
  })
}

// A dragged control sends at most once a frame, its latest value.
const latest = new Map<string, Command>()
let flushQueued = false

export function actLatest(key: string, command: Command) {
  latest.set(key, command)
  if (flushQueued) return
  flushQueued = true
  requestAnimationFrame(() => {
    flushQueued = false
    const commands = [...latest.values()]
    latest.clear()
    for (const c of commands) act(c)
  })
}

export async function loadInstalled() {
  const reply = await request({ cmd: 'installed' })
  if (reply.ok) setState({ installed: (reply.installed as InstalledEffect[]) ?? [] })
  else notify(reply.error ?? 'could not read the plug-in catalog', true)
}
