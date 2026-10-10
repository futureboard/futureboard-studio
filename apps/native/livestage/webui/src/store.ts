// The page's one connection to the server and everything it has been told.
//
// Session, status and the rest are React state through `useStore`. Meters
// are not: they arrive thirty times a second, so they go into a plain map
// that the meter canvases read on their own animation frame.

import { useSyncExternalStore } from 'react'
import type {
  AuthMessage,
  BusStrip,
  ChannelStrip,
  Command,
  Hello,
  History,
  Id,
  InsertState,
  InstalledEffect,
  LibraryItem,
  MatrixStrip,
  MidiMap,
  RemoteSettings,
  RemoteStatus,
  ServerMessage,
  Session,
  StripLevels,
  StatusMessage,
  StorageMessage,
  TelemetryFrame,
} from './protocol.ts'
import { MONITOR_KEY, READ_ONLY_COMMANDS, TALKBACK_KEY, stripKey } from './protocol.ts'
import { hasConsoleCore, hasPhase3, hasPlayback, hasScenes, normalizeSession } from './processing.ts'
import { storageDemo } from './storageDemo.ts'

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
  /** The appliance's disks, or why there are none to manage here. */
  storage: StorageMessage | null
  /** Whether the server's session carries the console core (processing,
   *  DCAs, mute groups, monitor); null until a session arrives. Without it
   *  the page shows the engine's defaults and says the controls do nothing. */
  consoleCore: boolean | null
  /** Whether the server's session carries scenes, and with them recall
   *  safe, undo/redo and paste (Phase 2); null until a session arrives. */
  phase2: boolean | null
  /** Whether the server's session carries Phase 3 (bus roles and width,
   *  per-send pan, matrices, talkback, the oscillator, the monitor source,
   *  layers); null until a session arrives. */
  phase3: boolean | null
  /** What Undo and Redo would do; null until the server says. */
  history: History | null
  /** The current scene and whether the mix has moved from it since. */
  sceneState: SceneState | null
  /** The strip library; null while the server has sent none. */
  library: LibraryItem[] | null
  saved: SaveState
  notice: Notice | null
  /** Who this page is, as the server last said (Phase 4); null until an
   *  `auth` arrives. An older server never sends one: open mode. */
  auth: AuthMessage | null
  /** A kept token is being offered to the server (`resume`): the login
   *  screen waits for the answer instead of flashing up. */
  resuming: boolean
  /** Whether the server's session carries playback (Phase 4); null until a
   *  session arrives. */
  phase4: boolean | null
  /** What the last MIDI learn produced (null `map`: refused, see `error`),
   *  and when (ms). */
  learned: { map: MidiMap | null; error: string | null; at: number } | null
  /** The remote settings and their status as last pushed (admins only). */
  remote: { settings: RemoteSettings; status: RemoteStatus | null; at: number } | null
}

export interface SceneState {
  current: Id | null
  modified: boolean
}

export interface SaveState {
  /** When the session file was last written (ms since the epoch), as far as
   *  this page has heard. */
  at: number | null
  path: string | null
  auto: boolean
  /** The session changed since that save; null: not known (nothing changed
   *  or saved since this page connected). */
  dirty: boolean | null
}

type Reply = { ok: boolean; error?: string } & Record<string, unknown>

let state: State = {
  connection: 'connecting',
  hello: null,
  session: null,
  status: null,
  insertStates: {},
  installed: null,
  storage: null,
  consoleCore: null,
  phase2: null,
  phase3: null,
  history: null,
  sceneState: null,
  library: null,
  saved: { at: null, path: null, auto: false, dirty: null },
  notice: null,
  auth: null,
  resuming: false,
  phase4: null,
  learned: null,
  remote: null,
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

/** A refusal in words: an older server answers a command it has never heard
 *  of with serde's "unknown variant `undo`, expected one of …". */
export function explain(error: string | undefined): string {
  if (error === 'locked') return 'Locked: unlock with a PIN to change anything.'
  const unknown = /unknown variant `(\w+)`/.exec(error ?? '')
  if (unknown) return `This LiveStage server does not know “${unknown[1]}”: it predates this feature. Update the server.`
  return error ?? 'refused'
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
  /** The processing section's reduction, dB (>= 0), held like a peak; 0
   *  from a server that does not report it. */
  gateDb: number
  compDb: number
  gateOpen: boolean
}

export const meters = new Map<string, MeterState>()

function meterFor(key: string): MeterState {
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
      gateDb: 0,
      compDb: 0,
      gateOpen: true,
    }
    meters.set(key, meter)
  }
  return meter
}

function takeMeters(update: Extract<ServerMessage, { type: 'meters' }>) {
  const now = performance.now()
  // The monitor bus: an output-only meter under its own key.
  if (Array.isArray(update.monitor)) {
    const levels = update.monitor
    takeMeter(meterFor(MONITOR_KEY), { output: [levels[0] ?? 0, levels[1] ?? 0], input: [0, 0] }, now)
  }
  // The talkback input: mono, on both sides of an output-only meter.
  if (Array.isArray(update.talkback)) {
    const peak = update.talkback[0] ?? 0
    takeMeter(meterFor(TALKBACK_KEY), { output: [peak, peak], input: [0, 0] }, now)
  }
  for (const { strip, levels } of update.meters) takeMeter(meterFor(stripKey(strip)), levels, now)
}

function takeMeter(meter: MeterState, levels: StripLevels, now: number) {
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
  // Absent from an older server: nothing taken off, gate open.
  meter.gateDb = Math.max(meter.gateDb, levels.gate_db ?? 0)
  meter.compDb = Math.max(meter.compDb, levels.comp_db ?? 0)
  meter.gateOpen = levels.gate_open ?? true
}

// ── Session updates ─────────────────────────────────────────────────────

/** Keep the previous object for every strip that did not change, so a
 *  memoised strip only redraws when it is the one that moved. */
function share(previous: Session | null, next: Session): Session {
  if (!previous) return next
  const reuse = <T extends ChannelStrip | BusStrip | MatrixStrip>(old: T[], fresh: T[]): T[] =>
    fresh.map((strip) => {
      const before = old.find((o) => o.id === strip.id)
      return before && JSON.stringify(before) === JSON.stringify(strip) ? before : strip
    })
  const keep = <T>(old: T, fresh: T): T => (JSON.stringify(old) === JSON.stringify(fresh) ? old : fresh)
  return {
    ...next,
    channels: reuse(previous.channels, next.channels),
    buses: reuse(previous.buses, next.buses),
    master: keep(previous.master, next.master),
    outputs: keep(previous.outputs, next.outputs),
    dcas: keep(previous.dcas, next.dcas),
    mute_groups: keep(previous.mute_groups, next.mute_groups),
    monitor: keep(previous.monitor, next.monitor),
    scenes: keep(previous.scenes, next.scenes),
    matrices: reuse(previous.matrices, next.matrices),
    talkback: keep(previous.talkback, next.talkback),
    oscillator: keep(previous.oscillator, next.oscillator),
    layers: keep(previous.layers, next.layers),
  }
}

// What the saved indicator compares: the session as last sent, sessions seen
// since this connection opened, and revisions where the server sends them.
let lastSessionText: string | null = null
let sessionsSinceOpen = 0
let sessionRevision: number | null = null
let savedRevision: number | null = null

function takeSession(message: Extract<ServerMessage, { type: 'session' }>) {
  const text = JSON.stringify(message.session)
  const changed = lastSessionText !== null && text !== lastSessionText
  lastSessionText = text
  sessionsSinceOpen += 1
  const revision = (message as { revision?: unknown }).revision
  if (typeof revision === 'number') sessionRevision = revision
  let dirty = state.saved.dirty
  if (sessionRevision !== null && savedRevision !== null) dirty = sessionRevision > savedRevision
  // The first session after (re)connecting is where things stand, not a change.
  else if (changed && sessionsSinceOpen > 1) dirty = true
  // An older server's session lacks the console core and scenes: filled with
  // the engine's defaults, so nothing downstream meets a missing field.
  setState({
    session: share(state.session, normalizeSession(message.session)),
    consoleCore: hasConsoleCore(message.session),
    phase2: hasScenes(message.session),
    phase3: hasPhase3(message.session),
    phase4: hasPlayback(message.session),
    saved: dirty === state.saved.dirty ? state.saved : { ...state.saved, dirty },
  })
}

/** The session file was written (by Save, autosave, or a scene store). */
export function markSaved(path: string | null, at: number, auto: boolean, revision?: number) {
  if (typeof revision === 'number') savedRevision = revision
  const dirty = sessionRevision !== null && savedRevision !== null ? sessionRevision > savedRevision : false
  setState({ saved: { at, path, auto, dirty } })
}

function receive(message: ServerMessage) {
  switch (message.type) {
    case 'auth':
      takeAuth(message)
      break
    case 'remote':
      if ('learned' in message) {
        setState({ learned: { map: message.learned ?? null, error: message.error ?? null, at: Date.now() } })
      }
      if (message.remote) {
        setState({ remote: { settings: message.remote, status: message.status ?? null, at: Date.now() } })
      }
      break
    case 'hello':
      setState({ hello: message })
      break
    case 'session':
      takeSession(message)
      break
    case 'history':
      setState({
        history: {
          undo: message.undo ?? null,
          redo: message.redo ?? null,
          undo_depth: message.undo_depth ?? (message.undo ? 1 : 0),
          redo_depth: message.redo_depth ?? (message.redo ? 1 : 0),
        },
      })
      break
    case 'scene_state':
      setState({ sceneState: { current: message.current ?? null, modified: message.modified === true } })
      break
    case 'library':
      setState({ library: Array.isArray(message.items) ? message.items : [] })
      break
    case 'saved': {
      const at = Date.parse(message.at)
      markSaved(message.path ?? null, Number.isFinite(at) ? at : Date.now(), message.auto === true, message.revision)
      break
    }
    case 'status': {
      const patch: Partial<State> = { status: message }
      if (JSON.stringify(message.inserts) !== JSON.stringify(state.insertStates)) {
        patch.insertStates = message.inserts
      }
      // A Phase 2 server says outright when it last saved and whether the
      // mix moved since; that outranks what the page pieced together.
      if (typeof message.unsaved === 'boolean' || message.last_saved !== undefined) {
        const last = message.last_saved ?? null
        const at = last ? Date.parse(last.at) : NaN
        const saved: SaveState = {
          at: Number.isFinite(at) ? at : state.saved.at,
          path: last?.path ?? state.saved.path,
          auto: last ? last.auto === true : state.saved.auto,
          dirty: typeof message.unsaved === 'boolean' ? message.unsaved : state.saved.dirty,
        }
        if (JSON.stringify(saved) !== JSON.stringify(state.saved)) patch.saved = saved
      }
      setState(patch)
      break
    }
    case 'storage':
      // Dev builds only: `?storageDemo` draws the Storage card from a canned
      // appliance state (there is no storage service on a dev machine).
      setState({
        storage: import.meta.env.DEV && location.search.includes('storageDemo') ? storageDemo(message) : message,
      })
      break
    case 'meters':
      takeMeters(message)
      break
    case 'telemetry':
      for (const handler of telemetryHandlers) handler(message.insert, message.frame)
      break
    case 'reply': {
      const key = typeof message.req === 'number' ? message.req : message.id
      if (typeof key === 'number') {
        const resolve = pending.get(key)
        pending.delete(key)
        resolve?.(message)
      }
      break
    }
  }
}

// ── Users, login and lock (Phase 4) ─────────────────────────────────────

const TOKEN_KEY = 'livestage.token'

/** The token this device keeps for `resume` (a login, or an open-mode lock). */
function savedToken(): string | null {
  try {
    return localStorage.getItem(TOKEN_KEY)
  } catch {
    return null
  }
}

let memoryToken: string | null = savedToken()

/** Keep (or forget, with null) the token sent with `resume` on every
 *  (re)connect. Blocked storage: it lasts until the page reloads. */
export function keepToken(token: string | null) {
  memoryToken = token
  try {
    if (token) localStorage.setItem(TOKEN_KEY, token)
    else localStorage.removeItem(TOKEN_KEY)
  } catch {
    // Blocked storage: kept in memory only.
  }
}

function takeAuth(message: AuthMessage) {
  const auth: AuthMessage = {
    type: 'auth',
    mode: message.mode === 'users' ? 'users' : 'open',
    users: Array.isArray(message.users) ? message.users : [],
    user: message.user ? { ...message.user, mixes: Array.isArray(message.user.mixes) ? message.user.mixes : [] } : null,
    locked: message.locked === true,
  }
  const patch: Partial<State> = { auth }
  // Logged out (or never in): what an earlier login showed must not linger.
  if (auth.mode === 'users' && !auth.user) {
    lastSessionText = null
    Object.assign(patch, {
      session: null,
      status: null,
      history: null,
      sceneState: null,
      library: null,
      storage: null,
      installed: null,
      remote: null,
      insertStates: {},
    } satisfies Partial<State>)
    meters.clear()
  }
  setState(patch)
}

/** Offer the kept token: logs this connection back in (and back into a
 *  lock). A token the server no longer knows is forgotten quietly. */
function resume() {
  const token = memoryToken
  if (!token) return
  setState({ resuming: true })
  void request({ cmd: 'resume', token }).then((reply) => {
    if (!reply.ok && reply.error !== 'disconnected' && reply.error !== 'not connected') {
      keepToken(null)
      // Users mode: say why the PIN is asked again (an open-mode lock's
      // token lapsing needs no word).
      if (state.auth?.mode === 'users') notify(explain(reply.error), true)
    }
    setState({ resuming: false })
  })
}

/** Log in; the token is kept for the next (re)connect. Null, or why not. */
export async function login(name: string, pin: string): Promise<string | null> {
  const reply = await request({ cmd: 'login', name, pin })
  if (!reply.ok) return explain(reply.error)
  if (typeof reply.token === 'string') keepToken(reply.token)
  return null
}

export async function logout() {
  keepToken(null)
  await request({ cmd: 'logout' })
}

/** Lock this page (open mode: with a PIN chosen now). Null, or why not. */
export async function lock(pin?: string): Promise<string | null> {
  const reply = await request(pin === undefined ? { cmd: 'lock' } : { cmd: 'lock', pin })
  if (!reply.ok) return explain(reply.error)
  // Open mode issues a token, so a reload stays locked after `resume`.
  if (typeof reply.token === 'string') keepToken(reply.token)
  return null
}

export async function unlock(pin: string): Promise<string | null> {
  const reply = await request({ cmd: 'unlock', pin })
  if (!reply.ok) return explain(reply.error)
  // Open mode: the token was the lock alone, and the server forgets it.
  if (state.auth?.mode !== 'users') keepToken(null)
  return null
}

/** Whether the server refused because of who this is (role or lock). */
export function isDenied(reply: { ok: boolean; error?: string; denied?: unknown }): boolean {
  return !reply.ok && (reply.denied === true || reply.error === 'locked' || /^not allowed/.test(reply.error ?? ''))
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
// Far above any session id: a server that echoes a command's own `id` (a
// scene's) as the reply's can never be taken for answering another request.
let nextId = 1_000_000
const pending = new Map<number, (reply: Reply) => void>()
/** How long a command that carries its own `id` waits for its answer. */
const OWN_ID_TIMEOUT_MS = 8000

export function connect() {
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws'
  const ws = new WebSocket(`${scheme}://${location.host}/ws`)
  socket = ws
  setState({ connection: 'connecting' })
  ws.onopen = () => {
    sessionsSinceOpen = 0
    setState({ connection: 'open' })
    // First, so everything after it is sent as the logged-in user.
    resume()
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
  // What the server would refuse anyway, refused here without a round
  // trip: anything that changes something, from a viewer or while locked.
  const auth = state.auth
  if (auth && !READ_ONLY_COMMANDS.has(command.cmd)) {
    if (auth.locked && command.cmd !== 'lock') return Promise.resolve({ ok: false, error: 'locked', denied: true })
    if (auth.user?.role === 'viewer') {
      return Promise.resolve({ ok: false, error: 'not allowed (viewer)', denied: true })
    }
  }
  const id = nextId++
  // The request's own number travels as `id`, which the server echoes in
  // its reply — except on the scene commands, whose `id` is their subject
  // (and optional on `scene_store`, where a stray request number would name
  // a scene): there the request goes as `req`.
  const ownId = 'id' in command || command.cmd.startsWith('scene_')
  socket.send(JSON.stringify(ownId ? { ...command, req: id } : { ...command, id }))
  return new Promise((resolve) => {
    pending.set(id, resolve)
    if (!ownId) return
    // A server that does not know `req` never answers this one by number.
    window.setTimeout(() => {
      if (!pending.has(id)) return
      pending.delete(id)
      resolve({ ok: false, error: 'The server did not answer (it may not support scene commands yet)' })
    }, OWN_ID_TIMEOUT_MS)
  })
}

/** Send a command; say so if the server refused it. */
export function act(command: Command) {
  void request(command).then((reply) => {
    if (!reply.ok) notify(explain(reply.error), true)
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

/** Send a command at most once per `intervalMs` while it keeps changing,
 *  always ending on the latest: a knob or a graph node dragged on a
 *  processing section, whose command carries the whole section. */
const paced = new Map<string, { command: Command | null; last: number; timer: number | undefined }>()

export function actPaced(key: string, command: Command, intervalMs = 25) {
  let entry = paced.get(key)
  if (!entry) {
    entry = { command: null, last: -Infinity, timer: undefined }
    paced.set(key, entry)
  }
  entry.command = command
  if (entry.timer !== undefined) return
  const pending = entry
  pending.timer = window.setTimeout(
    () => {
      pending.timer = undefined
      pending.last = performance.now()
      const next = pending.command
      pending.command = null
      if (next) act(next)
    },
    Math.max(0, pending.last + intervalMs - performance.now()),
  )
}

export async function loadInstalled() {
  const reply = await request({ cmd: 'installed' })
  if (reply.ok) setState({ installed: (reply.installed as InstalledEffect[]) ?? [] })
  else notify(reply.error ?? 'could not read the plug-in catalog', true)
}
