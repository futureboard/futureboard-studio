// The console core as the page holds it: every strip's processing section,
// the DCAs, mute groups and the monitor, with the engine's own defaults and
// ranges (crates/LiveStageEngine/src/processing.rs, session.rs).
//
// A session from a server that predates the console core lacks these
// fields; `normalizeSession` fills them as serde's `#[serde(default)]` would,
// so the rest of the page always sees whole objects and never trips on a
// missing one.

import type {
  BusRole,
  BusStrip,
  ChannelStrip,
  Comp,
  Dca,
  Delay,
  Destination,
  Eq,
  EqBand,
  EqKind,
  Gate,
  Hpf,
  Layer,
  MasterStrip,
  MatrixSend,
  MatrixSource,
  MatrixStrip,
  Monitor,
  MonitorSource,
  MuteGroup,
  Oscillator,
  OscillatorKind,
  PlaybackSetup,
  PlaybackTrack,
  Processing,
  ProcessingOrder,
  RecallScope,
  SceneSummary,
  Session,
  SoloMode,
  StripRef,
  Talkback,
} from './protocol.ts'
import { DCA_COUNT, LAYER_COUNT, MUTE_GROUP_COUNT, STRIP_COLORS } from './protocol.ts'

// ── Ranges (the doc comments in processing.rs) ──────────────────────────

export const HPF_HZ: [number, number] = [20, 600]
export const HPF_SLOPES = [12, 18, 24] as const
export const GATE_THRESHOLD_DB: [number, number] = [-80, 0]
export const GATE_RANGE_DB: [number, number] = [-80, 0]
export const GATE_ATTACK_MS: [number, number] = [0.05, 100]
export const GATE_HOLD_MS: [number, number] = [0, 2000]
export const GATE_RELEASE_MS: [number, number] = [5, 4000]
export const EQ_HZ: [number, number] = [20, 20000]
export const EQ_GAIN_DB: [number, number] = [-18, 18]
export const EQ_Q: [number, number] = [0.1, 10]
export const COMP_THRESHOLD_DB: [number, number] = [-60, 0]
export const COMP_RATIO: [number, number] = [1, 20]
export const COMP_ATTACK_MS: [number, number] = [0.1, 200]
export const COMP_RELEASE_MS: [number, number] = [10, 2000]
export const COMP_KNEE_DB: [number, number] = [0, 24]
export const COMP_MAKEUP_DB: [number, number] = [0, 24]
/** processing::MAX_DELAY_MS */
export const MAX_DELAY_MS = 1000
/** The speed of sound the delay's distance is read at. */
export const SOUND_M_PER_MS = 0.343
export const EQ_BANDS = 4

// ── Defaults (the `Default` impls in processing.rs) ─────────────────────

export function defaultHpf(): Hpf {
  return { on: false, hz: 80, slope_db: 12 }
}

export function defaultGate(): Gate {
  return { on: false, threshold_db: -50, range_db: -80, attack_ms: 0.5, hold_ms: 20, release_ms: 150 }
}

/** `EqBand::default()`: what serde fills a band's missing fields from. */
export function defaultEqBand(): EqBand {
  return { kind: 'bell', hz: 1000, gain_db: 0, q: 1 }
}

export function defaultEq(): Eq {
  return {
    on: true,
    bands: [
      { kind: 'low_shelf', hz: 100, gain_db: 0, q: 0.7 },
      { kind: 'bell', hz: 400, gain_db: 0, q: 1 },
      { kind: 'bell', hz: 2500, gain_db: 0, q: 1 },
      { kind: 'high_shelf', hz: 8000, gain_db: 0, q: 0.7 },
    ],
  }
}

export function defaultComp(): Comp {
  return { on: false, threshold_db: -20, ratio: 3, attack_ms: 10, release_ms: 150, knee_db: 6, makeup_db: 0 }
}

export function defaultDelay(): Delay {
  return { on: false, ms: 0 }
}

export function defaultProcessing(): Processing {
  return {
    hpf: defaultHpf(),
    gate: defaultGate(),
    eq: defaultEq(),
    comp: defaultComp(),
    delay: defaultDelay(),
    order: 'eq_then_comp',
  }
}

export const defaultMonitor = (): Monitor => ({ solo_mode: 'pfl', level_db: 0, dim: false, source: { kind: 'master' } })
export const defaultDca = (index: number): Dca => ({ name: `DCA ${index + 1}`, level_db: 0, mute: false, color: null })
export const defaultMuteGroup = (index: number): MuteGroup => ({ name: `Mute ${index + 1}`, active: false })

// ── Reading what the server sent ────────────────────────────────────────

type Loose = Record<string, unknown>

const isObject = (v: unknown): v is Loose => typeof v === 'object' && v !== null && !Array.isArray(v)
const num = (v: unknown, fallback: number) => (typeof v === 'number' && Number.isFinite(v) ? v : fallback)
const bool = (v: unknown, fallback: boolean) => (typeof v === 'boolean' ? v : fallback)
const str = (v: unknown, fallback: string) => (typeof v === 'string' ? v : fallback)

/** Each field of `fallback` from `raw` where it has one of the right type
 *  (serde's `#[serde(default)]` on a flat struct). */
function fill<T extends object>(raw: unknown, fallback: T): T {
  if (!isObject(raw)) return fallback
  const out = { ...fallback } as Loose
  for (const [key, value] of Object.entries(fallback)) {
    const given = raw[key]
    if (typeof value === 'number') out[key] = num(given, value)
    else if (typeof value === 'boolean') out[key] = bool(given, value)
    else if (typeof value === 'string') out[key] = str(given, value)
  }
  return out as T
}

const EQ_KINDS: EqKind[] = ['low_shelf', 'bell', 'high_shelf']
const ORDERS: ProcessingOrder[] = ['eq_then_comp', 'comp_then_eq']
const SOLO_MODES: SoloMode[] = ['pfl', 'afl', 'sip']

function readBand(raw: unknown, fallback: EqBand): EqBand {
  const band = fill(raw, fallback)
  if (!EQ_KINDS.includes(band.kind)) band.kind = fallback.kind
  return band
}

function readEq(raw: unknown): Eq {
  const fallback = defaultEq()
  if (!isObject(raw)) return fallback
  const given: unknown[] = Array.isArray(raw.bands) ? raw.bands : []
  // A band that is there takes its missing fields from `EqBand::default()`;
  // one that is not there at all is the EQ's own default band.
  const bands = fallback.bands.map((band, i) => (i < given.length ? readBand(given[i], defaultEqBand()) : band))
  return { on: bool(raw.on, fallback.on), bands }
}

export function readProcessing(raw: unknown): Processing {
  if (!isObject(raw)) return defaultProcessing()
  const order = ORDERS.includes(raw.order as ProcessingOrder) ? (raw.order as ProcessingOrder) : 'eq_then_comp'
  return {
    hpf: fill(raw.hpf, defaultHpf()),
    gate: fill(raw.gate, defaultGate()),
    eq: readEq(raw.eq),
    comp: fill(raw.comp, defaultComp()),
    delay: fill(raw.delay, defaultDelay()),
    order,
  }
}

const readColor = (v: unknown): number | null =>
  typeof v === 'number' && Number.isInteger(v) && v >= 0 && v < STRIP_COLORS ? v : null

const readIndices = (v: unknown, count: number): number[] =>
  Array.isArray(v)
    ? [...new Set(v.filter((i): i is number => typeof i === 'number' && Number.isInteger(i) && i >= 0 && i < count))]
    : []

function readCore<T extends ChannelStrip | BusStrip | MasterStrip | MatrixStrip>(raw: T): T {
  const loose = raw as unknown as Loose
  return {
    ...raw,
    processing: readProcessing(loose.processing),
    color: readColor(loose.color),
    dcas: readIndices(loose.dcas, DCA_COUNT),
    mute_groups: readIndices(loose.mute_groups, MUTE_GROUP_COUNT),
    solo_safe: bool(loose.solo_safe, false),
    recall_safe: bool(loose.recall_safe, false),
  }
}

/** `RecallScope::default()`: a recall touches everything. */
export function defaultScope(): RecallScope {
  return {
    input: true,
    processing: true,
    inserts: true,
    faders: true,
    mutes: true,
    pan: true,
    sends: true,
    routing: true,
    assign: true,
    names: true,
  }
}

function readScenes(raw: unknown): SceneSummary[] {
  if (!Array.isArray(raw)) return []
  return raw
    .filter((s): s is Loose => isObject(s) && typeof s.id === 'number')
    .map((s) => ({
      id: s.id as number,
      name: str(s.name, 'Scene'),
      note: str(s.note, ''),
      scope: fill(s.scope, defaultScope()),
    }))
}

/** Whether a session as sent carries scenes (and recall safe, undo and
 *  paste with them): a server that predates Phase 2 has no `scenes`. */
export function hasScenes(raw: unknown): boolean {
  return isObject(raw) && Array.isArray(raw.scenes)
}

/** Exactly `count` entries: what is there, padded with defaults. */
function padded<T extends object>(raw: unknown, count: number, fallback: (i: number) => T): T[] {
  const list = Array.isArray(raw) ? raw : []
  return Array.from({ length: count }, (_, i) => {
    const entry = fill(list[i], fallback(i))
    return 'color' in entry ? { ...entry, color: readColor((list[i] as Loose | undefined)?.color) } : entry
  })
}

/** Whether a session as sent carries the console core at all (a server
 *  that predates it has none of these). */
export function hasConsoleCore(raw: unknown): boolean {
  return isObject(raw) && Array.isArray(raw.dcas) && Array.isArray(raw.mute_groups) && isObject(raw.monitor)
}

// ── Bus & Monitor (Phase 3) ─────────────────────────────────────────────

const ROLES: BusRole[] = ['aux', 'group', 'fx']
const OSC_KINDS: OscillatorKind[] = ['sine', 'pink', 'white']

export const defaultTalkback = (): Talkback => ({ input: null, level_db: 0, hpf: true, to: [] })
export const defaultOscillator = (): Oscillator => ({ kind: 'sine', hz: 1000, level_db: -20, to: [] })
/** Oscillator level range, dBFS (−90 is off). */
export const OSC_LEVEL_DB: [number, number] = [-90, 0]
export const OSC_HZ: [number, number] = [20, 20000]

const isId = (v: unknown): v is number => typeof v === 'number' && Number.isInteger(v)

/** `{kind, id?}` with a kind from `kinds`; null when it is not one. */
function readRef<T extends { kind: string }>(raw: unknown, kinds: readonly string[], withId: readonly string[]): T | null {
  if (!isObject(raw) || typeof raw.kind !== 'string' || !kinds.includes(raw.kind)) return null
  if (!withId.includes(raw.kind)) return { kind: raw.kind } as T
  return isId(raw.id) ? ({ kind: raw.kind, id: raw.id } as unknown as T) : null
}

const readDestinations = (raw: unknown): Destination[] =>
  Array.isArray(raw)
    ? raw
        .map((d) => readRef<Destination>(d, ['bus', 'matrix', 'master', 'monitor'], ['bus', 'matrix']))
        .filter((d): d is Destination => d !== null)
    : []

export const readStripRef = (raw: unknown): StripRef | null =>
  readRef<StripRef>(raw, ['channel', 'bus', 'matrix', 'master'], ['channel', 'bus', 'matrix'])

function readBus(raw: BusStrip): BusStrip {
  const core = readCore(raw)
  const loose = raw as unknown as Loose
  // The engine's rule for a show saved before roles: a bus that feeds the
  // master is a group, anything else a monitor mix.
  const role = ROLES.includes(loose.role as BusRole)
    ? (loose.role as BusRole)
    : raw.output?.kind === 'master'
      ? 'group'
      : 'aux'
  return { ...core, role, stereo: bool(loose.stereo, true) }
}

function readChannel(raw: ChannelStrip): ChannelStrip {
  const core = readCore(raw)
  const sends = (raw.sends ?? []).map((s) => {
    const loose = s as unknown as Loose
    return { ...s, pan: num(loose.pan, 0), pan_follow: bool(loose.pan_follow, true) }
  })
  return { ...core, sends }
}

function readMatrix(raw: unknown): MatrixStrip | null {
  if (!isObject(raw) || !isId(raw.id)) return null
  const core = readCore({ fader_db: 0, pan: 0, mute: false, solo: false, inserts: [], ...raw } as unknown as MatrixStrip)
  const sources: MatrixSend[] = Array.isArray(raw.sources)
    ? raw.sources.flatMap((s): MatrixSend[] => {
        if (!isObject(s)) return []
        const source = readRef<MatrixSource>(s.source, ['master', 'bus'], ['bus'])
        return source ? [{ source, level_db: num(s.level_db, -90), pan: num(s.pan, 0) }] : []
      })
    : []
  return {
    ...core,
    id: raw.id,
    name: str(raw.name, 'Matrix'),
    stereo: bool(raw.stereo, true),
    sources,
    record_arm: bool(raw.record_arm, false),
  }
}

function readMonitorSource(raw: unknown): MonitorSource {
  return readRef<MonitorSource>(raw, ['master', 'bus', 'matrix'], ['bus', 'matrix']) ?? { kind: 'master' }
}

function readTalkback(raw: unknown): Talkback {
  const talkback = fill(raw, defaultTalkback())
  const input = isObject(raw) && isId(raw.input) && raw.input >= 0 ? raw.input : null
  return { ...talkback, input, to: readDestinations(isObject(raw) ? raw.to : null) }
}

function readOscillator(raw: unknown): Oscillator {
  const oscillator = fill(raw, defaultOscillator())
  if (!OSC_KINDS.includes(oscillator.kind)) oscillator.kind = 'sine'
  return { ...oscillator, to: readDestinations(isObject(raw) ? raw.to : null) }
}

function readLayers(raw: unknown): Layer[] {
  if (!Array.isArray(raw)) return []
  return raw.slice(0, LAYER_COUNT).flatMap((l, i): Layer[] => {
    if (!isObject(l)) return []
    const strips = Array.isArray(l.strips)
      ? l.strips.map(readStripRef).filter((s): s is StripRef => s !== null)
      : []
    return [{ name: str(l.name, `Layer ${i + 1}`), strips }]
  })
}

/** Whether a session as sent carries Phase 3 (bus roles, matrices,
 *  talkback, the oscillator, layers): an older server has no `matrices`. */
/** Phase 4's loaded take; nothing loaded from an older server. */
function readPlayback(raw: unknown): PlaybackSetup {
  if (!isObject(raw)) return { folder: null, tracks: [], virtual_soundcheck: false }
  const tracks = Array.isArray(raw.tracks)
    ? raw.tracks.flatMap((t): PlaybackTrack[] =>
        isObject(t) && typeof t.file === 'string'
          ? [{ file: t.file, channels: num(t.channels, 1), channel: typeof t.channel === 'number' ? t.channel : null }]
          : [],
      )
    : []
  return {
    folder: typeof raw.folder === 'string' ? raw.folder : null,
    tracks,
    virtual_soundcheck: bool(raw.virtual_soundcheck, false),
  }
}

/** Whether the server's session carries playback (Phase 4). */
export function hasPlayback(raw: unknown): boolean {
  return isObject(raw) && isObject(raw.playback)
}

export function hasPhase3(raw: unknown): boolean {
  return isObject(raw) && Array.isArray(raw.matrices)
}

/** The session with every console-core field present and in range. */
export function normalizeSession(raw: Session): Session {
  const loose = raw as unknown as Loose
  const monitor = fill(loose.monitor, defaultMonitor())
  if (!SOLO_MODES.includes(monitor.solo_mode)) monitor.solo_mode = 'pfl'
  monitor.source = readMonitorSource(isObject(loose.monitor) ? loose.monitor.source : null)
  return {
    ...raw,
    channels: (raw.channels ?? []).map(readChannel),
    buses: (raw.buses ?? []).map(readBus),
    master: readCore(raw.master),
    dcas: padded(loose.dcas, DCA_COUNT, defaultDca),
    mute_groups: padded(loose.mute_groups, MUTE_GROUP_COUNT, defaultMuteGroup),
    monitor,
    scenes: readScenes(loose.scenes),
    current_scene: typeof loose.current_scene === 'number' ? loose.current_scene : null,
    matrices: Array.isArray(loose.matrices)
      ? loose.matrices.map(readMatrix).filter((m): m is MatrixStrip => m !== null)
      : [],
    talkback: readTalkback(loose.talkback),
    oscillator: readOscillator(loose.oscillator),
    layers: readLayers(loose.layers),
    playback: readPlayback(loose.playback),
  }
}

// ── Helpers the views share ─────────────────────────────────────────────

export const clamp = (v: number, [lo, hi]: [number, number]) => Math.min(hi, Math.max(lo, v))

/** A deep copy to edit (Processing is plain data). */
export function cloneProcessing(p: Processing): Processing {
  return {
    hpf: { ...p.hpf },
    gate: { ...p.gate },
    eq: { on: p.eq.on, bands: p.eq.bands.map((b) => ({ ...b })) },
    comp: { ...p.comp },
    delay: { ...p.delay },
    order: p.order,
  }
}

export function sameProcessing(a: Processing, b: Processing): boolean {
  return JSON.stringify(a) === JSON.stringify(b)
}

/** `--fb-track-color-N` for a palette index; null for none. */
export function colorVar(index: number | null): string | null {
  return index === null ? null : `var(--fb-track-color-${index + 1})`
}

export function formatHz(hz: number): string {
  if (hz >= 1000) return `${(hz / 1000).toFixed(hz >= 10000 ? 1 : 2)}k`
  return `${Math.round(hz)}`
}

export function formatMs(ms: number): string {
  if (ms >= 1000) return `${(ms / 1000).toFixed(2)} s`
  if (ms < 1) return `${ms.toFixed(2)} ms`
  if (ms < 10) return `${ms.toFixed(1)} ms`
  return `${Math.round(ms)} ms`
}

export function formatSignedDb(db: number): string {
  if (Math.abs(db) < 0.05) return '0.0 dB'
  return `${db > 0 ? '+' : '−'}${Math.abs(db).toFixed(1)} dB`
}

/** The static compressor curve: soft knee of `knee_db` around the threshold,
 *  then makeup. Input and output in dBFS. */
export function compOutputDb(comp: Comp, input: number): number {
  const over = input - comp.threshold_db
  const knee = Math.max(0, comp.knee_db)
  const slope = 1 / Math.max(1, comp.ratio) - 1
  let out: number
  if (knee > 0 && Math.abs(over) <= knee / 2) out = input + (slope * (over + knee / 2) ** 2) / (2 * knee)
  else if (over > 0) out = input + slope * over
  else out = input
  return out + comp.makeup_db
}
