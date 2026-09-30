/**
 * Editor-side mirror of BurnLimit's parameter contract.
 *
 * Rust owns the authority: `src/ipc.rs` defines the ids, wire order and
 * ranges, and `src/lib.rs` the defaults and the style order. This module only
 * lays out controls and formats readouts. `tests/params.test.ts` pins every
 * number here against the Rust source.
 */

import { clamp } from './math'

export const PLUGIN_ID = 'burnlimit'

export const STYLES = ['clean', 'punch', 'modern', 'clip'] as const
export type Style = (typeof STYLES)[number]

/// Each style is a knee width and an attack time (`Style::knee_db` /
/// `Style::attack_sec`); the titles state them rather than describe a sound.
export const STYLE_OPTIONS: readonly { value: Style; label: string; title: string }[] = [
  { value: 'clean', label: 'Clean', title: '4 dB knee, 2 ms attack' },
  { value: 'punch', label: 'Punch', title: '2 dB knee, 0.8 ms attack' },
  { value: 'modern', label: 'Modern', title: '1.5 dB knee, 0.4 ms attack' },
  { value: 'clip', label: 'Clip', title: 'Near-hard 0.2 dB knee, 0.05 ms attack' },
]

export type BurnLimitParams = {
  power: boolean
  style: Style
  gainDb: number
  ceilingDb: number
  releaseMs: number
  lookaheadMs: number
  truePeak: boolean
  mix: number
  stereoLink: boolean
}

export type NumericId = 'gainDb' | 'ceilingDb' | 'releaseMs' | 'lookaheadMs' | 'mix'

/// `ipc::RANGES`, per continuous parameter.
export const RANGES: Record<NumericId, readonly [number, number]> = {
  gainDb: [-12, 24],
  ceilingDb: [-6, 0],
  releaseMs: [20, 2_000],
  lookaheadMs: [0, 10],
  mix: [0, 100],
}

/// `ipc::UI_PARAM_IDS`, in wire order.
export const PARAM_IDS = [
  'power',
  'style',
  'gainDb',
  'ceilingDb',
  'releaseMs',
  'lookaheadMs',
  'truePeak',
  'mix',
  'stereoLink',
] as const

/// `burnlimit::default_params()`.
export const DEFAULT_PARAMS: BurnLimitParams = {
  power: true,
  style: 'modern',
  gainDb: 0,
  ceilingDb: -0.3,
  releaseMs: 200,
  lookaheadMs: 2,
  truePeak: true,
  mix: 100,
  stereoLink: true,
}

/// `Style::to_wire`: the variant's position.
export const styleToWire = (style: Style) => STYLES.indexOf(style)

/// `Style::from_wire`: rounded, anything unknown is Clean.
export const styleFromWire = (value: number): Style => STYLES[Math.round(value)] ?? 'clean'

/// Every parameter as `(id, wire value)`, in wire order.
export function wireValues(params: BurnLimitParams): [string, number][] {
  return [
    ['power', params.power ? 1 : 0],
    ['style', styleToWire(params.style)],
    ['gainDb', params.gainDb],
    ['ceilingDb', params.ceilingDb],
    ['releaseMs', params.releaseMs],
    ['lookaheadMs', params.lookaheadMs],
    ['truePeak', params.truePeak ? 1 : 0],
    ['mix', params.mix],
    ['stereoLink', params.stereoLink ? 1 : 0],
  ]
}

const finite = (value: unknown): value is number => typeof value === 'number' && Number.isFinite(value)

/// Accept a host state blob only when every field is present and of the right
/// type — a half-applied state would show values the DSP does not have.
export function parseParams(state: unknown): BurnLimitParams | null {
  if (!state || typeof state !== 'object') return null
  const candidate = 'params' in state ? (state as { params?: unknown }).params : state
  if (!candidate || typeof candidate !== 'object') return null
  const p = candidate as Record<string, unknown>
  if (typeof p.power !== 'boolean' || typeof p.truePeak !== 'boolean' || typeof p.stereoLink !== 'boolean') {
    return null
  }
  if (!STYLES.includes(p.style as Style)) return null
  const ids = Object.keys(RANGES) as NumericId[]
  if (!ids.every((id) => finite(p[id]))) return null
  const numbers = Object.fromEntries(
    ids.map((id) => [id, clamp(p[id] as number, RANGES[id][0], RANGES[id][1])]),
  ) as Record<NumericId, number>
  return {
    power: p.power,
    style: p.style as Style,
    truePeak: p.truePeak,
    stereoLink: p.stereoLink,
    ...numbers,
  }
}

export function paramsMatch(left: BurnLimitParams, right: BurnLimitParams) {
  if (
    left.power !== right.power ||
    left.style !== right.style ||
    left.truePeak !== right.truePeak ||
    left.stereoLink !== right.stereoLink
  ) {
    return false
  }
  return (Object.keys(RANGES) as NumericId[]).every(
    (id) => Math.abs(left[id] - right[id]) <= Math.max(Math.abs(right[id]) * 0.002, 0.05),
  )
}
