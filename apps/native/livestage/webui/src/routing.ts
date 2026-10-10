// Bus & Monitor (Phase 3) as the page reads it: what a bus role means, the
// fader banks, strips found by reference, and where talkback and the
// oscillator can go. Pure functions over the session; no state.

import type {
  BusRole,
  BusStrip,
  ChannelStrip,
  Destination,
  MatrixSend,
  MatrixSource,
  MatrixStrip,
  SendSlot,
  Session,
  StripCore,
  StripRef,
} from './protocol.ts'
import { MIN_FADER_DB, sameStrip } from './protocol.ts'

// ── Bus roles ───────────────────────────────────────────────────────────

export interface RoleInfo {
  role: BusRole
  /** "Aux". */
  label: string
  /** The bank's name: "Aux", "Groups", "FX". */
  bank: string
  /** A new bus's name stem: "Aux 3". */
  stem: string
  /** A new send's default: pre-fader for a monitor mix. */
  preFader: boolean
  /** What the role is for and what it defaults to. */
  detail: string
}

export const ROLES: RoleInfo[] = [
  {
    role: 'aux',
    label: 'Aux',
    bank: 'Aux',
    stem: 'Aux',
    preFader: true,
    detail: 'A monitor mix for a wedge or IEM. New sends are pre-fader, so the FOH fader never moves it; its output starts unpatched (Patch → Outputs).',
  },
  {
    role: 'group',
    label: 'Group',
    bank: 'Groups',
    stem: 'Group',
    preFader: false,
    detail: 'A subgroup: channels route to it as their output or by post-fader sends; it feeds the master.',
  },
  {
    role: 'fx',
    label: 'FX',
    bank: 'FX',
    stem: 'FX',
    preFader: false,
    detail: 'An effect send and its return in one strip: put the effect in its inserts. New sends are post-fader; it feeds the master.',
  },
]

export function roleInfo(role: BusRole): RoleInfo {
  return ROLES.find((r) => r.role === role) ?? ROLES[1]
}

/** A new name for a bus of `role` that no bus has yet: "Aux 3". */
export function nextBusName(session: Session, role: BusRole): string {
  const stem = roleInfo(role).stem
  const names = new Set(session.buses.map((b) => b.name.toLowerCase()))
  let n = session.buses.filter((b) => b.role === role).length + 1
  while (names.has(`${stem} ${n}`.toLowerCase())) n++
  return `${stem} ${n}`
}

export function nextMatrixName(session: Session): string {
  const names = new Set(session.matrices.map((m) => m.name.toLowerCase()))
  let n = session.matrices.length + 1
  while (names.has(`matrix ${n}`)) n++
  return `Matrix ${n}`
}

// ── Strips by reference ─────────────────────────────────────────────────

export interface FoundStrip {
  core: StripCore
  name: string
  channel: ChannelStrip | null
  bus: BusStrip | null
  matrix: MatrixStrip | null
}

export function findStrip(session: Session, strip: StripRef): FoundStrip | null {
  switch (strip.kind) {
    case 'master':
      return { core: session.master, name: 'Master', channel: null, bus: null, matrix: null }
    case 'channel': {
      const channel = session.channels.find((c) => c.id === strip.id)
      return channel ? { core: channel, name: channel.name, channel, bus: null, matrix: null } : null
    }
    case 'bus': {
      const bus = session.buses.find((b) => b.id === strip.id)
      return bus ? { core: bus, name: bus.name, channel: null, bus, matrix: null } : null
    }
    case 'matrix': {
      const matrix = session.matrices.find((m) => m.id === strip.id)
      return matrix ? { core: matrix, name: matrix.name, channel: null, bus: null, matrix } : null
    }
  }
}

/** "Channel", "Aux", "Group", "FX", "Matrix", "Master": what a strip is. */
export function kindLabel(session: Session, strip: StripRef): string {
  switch (strip.kind) {
    case 'master':
      return 'Master'
    case 'channel':
      return 'Channel'
    case 'matrix':
      return 'Matrix'
    case 'bus': {
      const bus = session.buses.find((b) => b.id === strip.id)
      return bus ? roleInfo(bus.role).label : 'Bus'
    }
  }
}

/** A matrix's solo is always a cue on the monitor (PFL, or AFL outside PFL
 *  mode): it never silences the main mix. */
export const MATRIX_SOLO = 'Solo: a cue on the monitor bus (PFL in PFL mode, else AFL). A matrix solo never silences the main mix'

// ── Fader banks ─────────────────────────────────────────────────────────

/** A built-in bank, or a custom layer by index. */
export type Bank = 'inputs' | 'aux' | 'groups' | 'fx' | 'matrix' | 'dcas' | { layer: number }

export const BUILTIN_BANKS: { bank: Exclude<Bank, { layer: number }>; label: string; title: string }[] = [
  { bank: 'inputs', label: 'Inputs', title: 'Input channels' },
  { bank: 'aux', label: 'Aux', title: 'Monitor mixes (aux buses)' },
  { bank: 'groups', label: 'Groups', title: 'Subgroups' },
  { bank: 'fx', label: 'FX', title: 'Effect sends and returns' },
  { bank: 'matrix', label: 'Matrix', title: 'Matrices: zone and record feeds from the master and buses' },
  { bank: 'dcas', label: 'DCAs', title: 'The eight DCA faders' },
]

export function bankKey(bank: Bank): string {
  return typeof bank === 'string' ? bank : `layer:${bank.layer}`
}

export function sameBank(a: Bank, b: Bank): boolean {
  return bankKey(a) === bankKey(b)
}

export function bankFromKey(key: string | null): Bank | null {
  if (!key) return null
  const match = /^layer:(\d)$/.exec(key)
  if (match) return { layer: Number(match[1]) }
  return BUILTIN_BANKS.some((b) => b.bank === key) ? (key as Bank) : null
}

/** The role a bus bank shows; null for the other banks. */
export function bankRole(bank: Bank): BusRole | null {
  return bank === 'aux' ? 'aux' : bank === 'groups' ? 'group' : bank === 'fx' ? 'fx' : null
}

/** Whether `strip` still exists in the session. */
export function stripExists(session: Session, strip: StripRef): boolean {
  return findStrip(session, strip) !== null
}

/** The strips a bank shows, in its order (the master is pinned apart). */
export function bankStrips(session: Session, bank: Bank): StripRef[] {
  if (typeof bank !== 'string') {
    const layer = session.layers[bank.layer]
    return layer ? layer.strips.filter((s) => s.kind !== 'master' && stripExists(session, s)) : []
  }
  if (bank === 'inputs') return session.channels.map((c) => ({ kind: 'channel', id: c.id }))
  if (bank === 'matrix') return session.matrices.map((m) => ({ kind: 'matrix', id: m.id }))
  const role = bankRole(bank)
  if (role) return session.buses.filter((b) => b.role === role).map((b) => ({ kind: 'bus', id: b.id }))
  return []
}

// ── Sends on Fader ──────────────────────────────────────────────────────

/** The bus or matrix whose sends the faders show. */
export type SofTarget = { kind: 'bus'; id: number } | { kind: 'matrix'; id: number }

/** A channel's send to `bus`, if it has one. */
export function sendTo(channel: ChannelStrip, bus: number): SendSlot | undefined {
  return channel.sends.find((s) => s.bus === bus)
}

/** A matrix's contribution from `source` (silent when it has none). */
export function matrixSend(matrix: MatrixStrip, source: MatrixSource): MatrixSend {
  return matrix.sources.find((s) => sameStrip(s.source, source)) ?? { source, level_db: MIN_FADER_DB, pan: 0 }
}

/** Every source a matrix can take, in console order: the master, then each
 *  bus. */
export function matrixSources(session: Session): { source: MatrixSource; name: string; detail: string }[] {
  return [
    { source: { kind: 'master' }, name: 'Master', detail: 'Main mix, after its fader' },
    ...session.buses.map((b) => ({
      source: { kind: 'bus', id: b.id } as MatrixSource,
      name: b.name,
      detail: `${roleInfo(b.role).label}${b.stereo ? '' : ' · mono'}, after its fader`,
    })),
  ]
}

// ── Talkback and oscillator destinations ────────────────────────────────

export function destinations(session: Session): { to: Destination; name: string; group: string }[] {
  return [
    { to: { kind: 'master' }, name: 'Master', group: 'Mix' },
    { to: { kind: 'monitor' }, name: 'Monitor', group: 'Mix' },
    ...session.buses.map((b) => ({ to: { kind: 'bus', id: b.id } as Destination, name: b.name, group: roleInfo(b.role).bank })),
    ...session.matrices.map((m) => ({ to: { kind: 'matrix', id: m.id } as Destination, name: m.name, group: 'Matrix' })),
  ]
}

/** `to` with `dest` added or taken out, in console order. */
export function toggleDestination(session: Session, to: Destination[], dest: Destination, on: boolean): Destination[] {
  const kept = to.filter((d) => !sameStrip(d, dest))
  if (!on) return kept
  const order = destinations(session).map((d) => d.to)
  const next = [...kept, dest]
  const rank = (d: Destination) => {
    const i = order.findIndex((o) => sameStrip(o, d))
    return i < 0 ? order.length : i
  }
  return next.sort((a, b) => rank(a) - rank(b))
}

/** Destinations that still exist (a removed bus leaves a stale one). */
export function liveDestinations(session: Session, to: Destination[]): Destination[] {
  const all = destinations(session).map((d) => d.to)
  return to.filter((d) => all.some((a) => sameStrip(a, d)))
}
