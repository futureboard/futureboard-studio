/**
 * Editor-side mirror of Transient's parameter contract.
 *
 * Rust owns the authority: `src/ipc.rs` defines the ids, wire order and
 * ranges, and `src/lib.rs` the defaults and the shaping depth. This module
 * only lays out controls and formats readouts. `tests/params.test.ts` pins
 * every number here against the Rust source.
 */

import { clamp } from './math'

export const PLUGIN_ID = 'transient'

/// `transient::MAX_SHAPE_DB`: the peak boost or cut at ±100 %.
export const MAX_SHAPE_DB = 18

export type TransientParams = {
  power: boolean
  attack: number
  sustain: number
  speed: number
  mix: number
  stereoLink: boolean
}

export type NumericId = 'attack' | 'sustain' | 'speed' | 'mix'

/// `ipc::RANGES`, per continuous parameter.
export const RANGES: Record<NumericId, readonly [number, number]> = {
  attack: [-100, 100],
  sustain: [-100, 100],
  speed: [0, 100],
  mix: [0, 100],
}

/// `ipc::UI_PARAM_IDS`, in wire order.
export const PARAM_IDS = ['power', 'attack', 'sustain', 'speed', 'mix', 'stereoLink'] as const

/// `transient::default_params()`.
export const DEFAULT_PARAMS: TransientParams = {
  power: true,
  attack: 0,
  sustain: 0,
  speed: 50,
  mix: 100,
  stereoLink: true,
}

/// Every parameter as `(id, wire value)`, in wire order.
export function wireValues(params: TransientParams): [string, number][] {
  return [
    ['power', params.power ? 1 : 0],
    ['attack', params.attack],
    ['sustain', params.sustain],
    ['speed', params.speed],
    ['mix', params.mix],
    ['stereoLink', params.stereoLink ? 1 : 0],
  ]
}

const finite = (value: unknown): value is number => typeof value === 'number' && Number.isFinite(value)

/// Accept a host state blob only when every field is present and of the right
/// type — a half-applied state would show values the DSP does not have.
export function parseParams(state: unknown): TransientParams | null {
  if (!state || typeof state !== 'object') return null
  const candidate = 'params' in state ? (state as { params?: unknown }).params : state
  if (!candidate || typeof candidate !== 'object') return null
  const p = candidate as Record<string, unknown>
  if (typeof p.power !== 'boolean' || typeof p.stereoLink !== 'boolean') return null
  const ids = Object.keys(RANGES) as NumericId[]
  if (!ids.every((id) => finite(p[id]))) return null
  const numbers = Object.fromEntries(
    ids.map((id) => [id, clamp(p[id] as number, RANGES[id][0], RANGES[id][1])]),
  ) as Record<NumericId, number>
  return { power: p.power, stereoLink: p.stereoLink, ...numbers }
}

export function paramsMatch(left: TransientParams, right: TransientParams) {
  if (left.power !== right.power || left.stereoLink !== right.stereoLink) return false
  return (Object.keys(RANGES) as NumericId[]).every((id) => Math.abs(left[id] - right[id]) < 0.05)
}

/// What a signed amount does to its region, in dB at the peak.
export function describeShape(amount: number) {
  const db = (amount / 100) * MAX_SHAPE_DB
  if (Math.abs(db) < 0.05) return 'Unchanged'
  return `${db > 0 ? '+' : '−'}${Math.abs(db).toFixed(1)} dB`
}
