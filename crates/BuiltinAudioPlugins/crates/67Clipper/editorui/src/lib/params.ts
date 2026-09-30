/**
 * Editor-side mirror of 67Clipper's parameter contract.
 *
 * Rust owns the authority: `src/ipc.rs` defines the ids, wire order and
 * ranges, and `src/lib.rs` the defaults and the mode order. This module only
 * lays out controls and formats readouts. `tests/params.test.ts` pins every
 * number here against the Rust source.
 */

import { clamp } from './math'

export const PLUGIN_ID = 'clipper67'

export const MODES = ['clip', 'hybrid', 'limit'] as const
export type Mode = (typeof MODES)[number]

/// All three drive into the same soft clipper; Hybrid and Limit add peak gain
/// reduction, Limit the most (see the crate docs).
export const MODE_OPTIONS: readonly { value: Mode; label: string; title: string }[] = [
  { value: 'clip', label: 'Clip', title: 'Soft clipper only' },
  { value: 'hybrid', label: 'Hybrid', title: 'Clipper plus peak gain reduction' },
  { value: 'limit', label: 'Limit', title: 'Clipper plus the strongest peak gain reduction' },
]

export type Clipper67Params = {
  power: boolean
  mode: Mode
  thresholdDb: number
  shape: number
  ceilingDb: number
  mix: number
  stereoLink: boolean
  dcFilter: boolean
}

export type NumericId = 'thresholdDb' | 'shape' | 'ceilingDb' | 'mix'

/// `ipc::RANGES`, per continuous parameter.
export const RANGES: Record<NumericId, readonly [number, number]> = {
  thresholdDb: [-24, 0],
  shape: [0, 100],
  ceilingDb: [-6, 0],
  mix: [0, 100],
}

/// `ipc::UI_PARAM_IDS`, in wire order.
export const PARAM_IDS = ['power', 'mode', 'thresholdDb', 'shape', 'ceilingDb', 'mix', 'stereoLink', 'dcFilter'] as const

/// `clipper67::default_params()`.
export const DEFAULT_PARAMS: Clipper67Params = {
  power: true,
  mode: 'clip',
  thresholdDb: -6,
  shape: 50,
  ceilingDb: -0.3,
  mix: 100,
  stereoLink: true,
  dcFilter: true,
}

/// `Mode::to_wire`: the variant's position.
export const modeToWire = (mode: Mode) => MODES.indexOf(mode)

/// `Mode::from_wire`: rounded, anything unknown is Clip.
export const modeFromWire = (value: number): Mode => MODES[Math.round(value)] ?? 'clip'

/// Every parameter as `(id, wire value)`, in wire order.
export function wireValues(params: Clipper67Params): [string, number][] {
  return [
    ['power', params.power ? 1 : 0],
    ['mode', modeToWire(params.mode)],
    ['thresholdDb', params.thresholdDb],
    ['shape', params.shape],
    ['ceilingDb', params.ceilingDb],
    ['mix', params.mix],
    ['stereoLink', params.stereoLink ? 1 : 0],
    ['dcFilter', params.dcFilter ? 1 : 0],
  ]
}

const finite = (value: unknown): value is number => typeof value === 'number' && Number.isFinite(value)

/// Accept a host state blob only when every field is present and of the right
/// type — a half-applied state would show values the DSP does not have.
export function parseParams(state: unknown): Clipper67Params | null {
  if (!state || typeof state !== 'object') return null
  const candidate = 'params' in state ? (state as { params?: unknown }).params : state
  if (!candidate || typeof candidate !== 'object') return null
  const p = candidate as Record<string, unknown>
  if (typeof p.power !== 'boolean' || typeof p.stereoLink !== 'boolean' || typeof p.dcFilter !== 'boolean') {
    return null
  }
  if (!MODES.includes(p.mode as Mode)) return null
  const ids = Object.keys(RANGES) as NumericId[]
  if (!ids.every((id) => finite(p[id]))) return null
  const numbers = Object.fromEntries(
    ids.map((id) => [id, clamp(p[id] as number, RANGES[id][0], RANGES[id][1])]),
  ) as Record<NumericId, number>
  return {
    power: p.power,
    mode: p.mode as Mode,
    stereoLink: p.stereoLink,
    dcFilter: p.dcFilter,
    ...numbers,
  }
}

export function paramsMatch(left: Clipper67Params, right: Clipper67Params) {
  if (
    left.power !== right.power ||
    left.mode !== right.mode ||
    left.stereoLink !== right.stereoLink ||
    left.dcFilter !== right.dcFilter
  ) {
    return false
  }
  return (Object.keys(RANGES) as NumericId[]).every((id) => Math.abs(left[id] - right[id]) < 0.05)
}
