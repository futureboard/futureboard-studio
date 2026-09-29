/**
 * Editor-side mirror of Imager's parameter contract.
 *
 * Rust owns the authority: `src/ipc.rs` defines the ids, ranges and clamping,
 * and `src/lib.rs` the defaults. This module exists so the editor can lay out
 * a control and format a readout — never to decide what a value means.
 * `params.test.ts` pins every number here against the Rust source.
 */

export const BAND_COUNT = 4
export const CROSSOVER_COUNT = BAND_COUNT - 1

/// `imager::MIN_CROSSOVER_HZ` / `MAX_CROSSOVER_HZ`.
export const MIN_CROSSOVER_HZ = 20
export const MAX_CROSSOVER_HZ = 20_000
/// `imager::DEFAULT_CROSSOVERS_HZ`.
export const DEFAULT_CROSSOVERS_HZ = [120, 1_000, 6_000] as const

/// `imager::MAX_WIDTH` / `DEFAULT_WIDTH`, in percent.
export const MAX_WIDTH = 200
export const DEFAULT_WIDTH = 100

/// `imager::SOLO_NONE`.
export const SOLO_NONE = -1

/// `imager::MIN_OUTPUT_DB` / `MAX_OUTPUT_DB`.
export const MIN_OUTPUT_DB = -24
export const MAX_OUTPUT_DB = 12

/// Crossovers are kept this far apart by the editor while one is dragged, so
/// no band can be squeezed to nothing. The DSP would still cope — it sorts —
/// but a zero-width band is not something anyone can grab again.
export const MIN_CROSSOVER_RATIO = 1.25

export type ImagerParams = {
  power: boolean
  crossoverHz: number[]
  width: number[]
  soloBand: number
  outputDb: number
}

/// Wire ids in `ipc::UI_PARAM_IDS` order.
export const PARAM_IDS = [
  'power',
  'crossover1Hz',
  'crossover2Hz',
  'crossover3Hz',
  'width1',
  'width2',
  'width3',
  'width4',
  'soloBand',
  'outputDb',
] as const

export type ParamId = (typeof PARAM_IDS)[number]

export const crossoverId = (index: number) => `crossover${index + 1}Hz` as ParamId
export const widthId = (index: number) => `width${index + 1}` as ParamId

export const DEFAULT_PARAMS: ImagerParams = {
  power: true,
  crossoverHz: [...DEFAULT_CROSSOVERS_HZ],
  width: Array.from({ length: BAND_COUNT }, () => DEFAULT_WIDTH),
  soloBand: SOLO_NONE,
  outputDb: 0,
}

export const BAND_NAMES = ['Low', 'Low Mid', 'High Mid', 'High'] as const

export function clamp(value: number, min: number, max: number) {
  return value < min ? min : value > max ? max : value
}

export function cloneParams(params: ImagerParams): ImagerParams {
  return {
    ...params,
    crossoverHz: [...params.crossoverHz],
    width: [...params.width],
  }
}

/// Crossovers low to high, the order the DSP runs them in
/// (`imager::effective_crossovers` sorts the same way).
export function sortedCrossovers(params: ImagerParams): number[] {
  return [...params.crossoverHz].sort((a, b) => a - b)
}

/// The range a crossover may be dragged over without passing a neighbour.
export function crossoverBounds(sorted: number[], index: number): [number, number] {
  const below = index > 0 ? sorted[index - 1]! * MIN_CROSSOVER_RATIO : MIN_CROSSOVER_HZ
  const above =
    index < sorted.length - 1 ? sorted[index + 1]! / MIN_CROSSOVER_RATIO : MAX_CROSSOVER_HZ
  return [Math.max(MIN_CROSSOVER_HZ, below), Math.min(MAX_CROSSOVER_HZ, above)]
}

export function formatHz(hz: number): string {
  if (hz >= 10_000) return `${(hz / 1000).toFixed(1)}k`
  if (hz >= 1000) return `${(hz / 1000).toFixed(2)}k`
  return `${Math.round(hz)}`
}

export function formatWidth(width: number): string {
  return `${Math.round(width)}`
}

export function formatDb(db: number): string {
  const rounded = Math.round(db * 10) / 10
  return `${rounded > 0 ? '+' : ''}${rounded.toFixed(1)}`
}

/// What a width does, in words, for the readout under a band.
export function describeWidth(width: number): string {
  if (width < 0.5) return 'Mono'
  if (width < 95) return 'Narrower'
  if (width <= 105) return 'Unchanged'
  return 'Wider'
}

/// The band `hz` falls in, given sorted crossovers.
export function bandAt(sorted: number[], hz: number): number {
  let band = 0
  while (band < sorted.length && hz >= sorted[band]!) band += 1
  return band
}

/// Log frequency axis shared by the band display and its hit-testing.
export function hzToUnit(hz: number): number {
  return (
    Math.log(clamp(hz, MIN_CROSSOVER_HZ, MAX_CROSSOVER_HZ) / MIN_CROSSOVER_HZ) /
    Math.log(MAX_CROSSOVER_HZ / MIN_CROSSOVER_HZ)
  )
}

export function unitToHz(unit: number): number {
  return MIN_CROSSOVER_HZ * Math.pow(MAX_CROSSOVER_HZ / MIN_CROSSOVER_HZ, clamp(unit, 0, 1))
}

/// Whole-state equality for preset matching, tolerant of float round-trips.
export function paramsMatch(left: ImagerParams, right: ImagerParams): boolean {
  if (left.power !== right.power || left.soloBand !== right.soloBand) return false
  if (Math.abs(left.outputDb - right.outputDb) > 0.05) return false
  const a = sortedCrossovers(left)
  const b = sortedCrossovers(right)
  for (let i = 0; i < CROSSOVER_COUNT; i++) {
    if (Math.abs(a[i]! - b[i]!) > Math.max(b[i]! * 0.002, 0.5)) return false
  }
  for (let i = 0; i < BAND_COUNT; i++) {
    if (Math.abs(left.width[i]! - right.width[i]!) > 0.25) return false
  }
  return true
}
