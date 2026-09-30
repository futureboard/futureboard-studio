/**
 * Editor-side mirror of the Compressor's parameter contract.
 *
 * Rust owns the authority: `src/ipc.rs` defines the ids, ranges and clamping,
 * and `src/lib.rs` the defaults and the gain curve. This module exists so the
 * editor can lay out a control, draw the curve and format a readout — never to
 * decide what a value means. `params.test.ts` pins every number here against
 * the Rust source.
 */

export const BAND_COUNT = 4
export const CROSSOVER_COUNT = BAND_COUNT - 1

/// `compresser::MIN_CROSSOVER_HZ` / `MAX_CROSSOVER_HZ` / `DEFAULT_CROSSOVERS_HZ`.
export const MIN_CROSSOVER_HZ = 20
export const MAX_CROSSOVER_HZ = 20_000
export const DEFAULT_CROSSOVERS_HZ = [120, 1_000, 5_000] as const

export const MIN_THRESHOLD_DB = -60
export const MAX_THRESHOLD_DB = 0
export const MIN_RATIO = 1
export const MAX_RATIO = 20
export const MAX_KNEE_DB = 24
export const MIN_ATTACK_MS = 0.1
export const MAX_ATTACK_MS = 200
export const MIN_RELEASE_MS = 5
export const MAX_RELEASE_MS = 2_000
export const MIN_MAKEUP_DB = -12
export const MAX_MAKEUP_DB = 24
export const MIN_OUTPUT_DB = -24
export const MAX_OUTPUT_DB = 12
/// The detector high-pass is off at or below this.
export const SIDECHAIN_OFF_HZ = 20
export const MAX_SIDECHAIN_HZ = 500

/// `compresser::SOLO_NONE`.
export const SOLO_NONE = -1

/// Crossovers are kept this far apart by the editor while one is dragged, so
/// no band can be squeezed to nothing. The DSP would still cope — it sorts —
/// but a zero-width band is not something anyone can grab again.
export const MIN_CROSSOVER_RATIO = 1.25

export type Mode = 'single' | 'multi'

export type Band = {
  thresholdDb: number
  ratio: number
  attackMs: number
  releaseMs: number
  makeupDb: number
  bypass: boolean
}

export type CompressorParams = {
  power: boolean
  mode: Mode
  thresholdDb: number
  ratio: number
  attackMs: number
  releaseMs: number
  makeupDb: number
  sidechainHpfHz: number
  kneeDb: number
  mix: number
  outputDb: number
  crossoverHz: number[]
  bands: Band[]
  soloBand: number
}

/// `compresser::DEFAULT_BANDS`, lowest band first.
export const DEFAULT_BANDS: readonly Band[] = [
  { thresholdDb: -20, ratio: 3, attackMs: 20, releaseMs: 200, makeupDb: 0, bypass: false },
  { thresholdDb: -20, ratio: 3, attackMs: 10, releaseMs: 150, makeupDb: 0, bypass: false },
  { thresholdDb: -20, ratio: 3, attackMs: 5, releaseMs: 100, makeupDb: 0, bypass: false },
  { thresholdDb: -20, ratio: 3, attackMs: 2, releaseMs: 80, makeupDb: 0, bypass: false },
]

/// `compresser::default_params()`.
export const DEFAULT_PARAMS: CompressorParams = {
  power: true,
  mode: 'single',
  thresholdDb: -18,
  ratio: 4,
  attackMs: 10,
  releaseMs: 100,
  makeupDb: 0,
  sidechainHpfHz: 0,
  kneeDb: 6,
  mix: 100,
  outputDb: 0,
  crossoverHz: [...DEFAULT_CROSSOVERS_HZ],
  bands: DEFAULT_BANDS.map((band) => ({ ...band })),
  soloBand: SOLO_NONE,
}

export const BAND_FIELDS = ['ThresholdDb', 'Ratio', 'AttackMs', 'ReleaseMs', 'MakeupDb', 'Bypass'] as const
export type BandField = (typeof BAND_FIELDS)[number]

/// Wire ids in `ipc::UI_PARAM_IDS` order.
export const PARAM_IDS = [
  'power',
  'mode',
  'thresholdDb',
  'ratio',
  'attackMs',
  'releaseMs',
  'makeupDb',
  'sidechainHpfHz',
  'kneeDb',
  'mix',
  'outputDb',
  'crossover1Hz',
  'crossover2Hz',
  'crossover3Hz',
  ...Array.from({ length: BAND_COUNT }, (_, band) =>
    BAND_FIELDS.map((field) => `band${band + 1}${field}`),
  ).flat(),
  'soloBand',
] as const

export const crossoverId = (index: number) => `crossover${index + 1}Hz`
export const bandParamId = (band: number, field: BandField) => `band${band + 1}${field}`

export const BAND_NAMES = ['Low', 'Low Mid', 'High Mid', 'High'] as const

export function clamp(value: number, min: number, max: number) {
  return value < min ? min : value > max ? max : value
}

export function cloneParams(params: CompressorParams): CompressorParams {
  return {
    ...params,
    crossoverHz: [...params.crossoverHz],
    bands: params.bands.map((band) => ({ ...band })),
  }
}

/// Reduction in dB (positive) the static curve asks for at `levelDb` — a
/// line-for-line copy of `compresser::curve_reduction_db`, used only to draw.
export function curveReductionDb(levelDb: number, thresholdDb: number, ratio: number, kneeDb: number) {
  const slope = 1 - 1 / Math.max(ratio, 1)
  const over = levelDb - thresholdDb
  const half = 0.5 * Math.max(kneeDb, 0)
  if (over <= -half) return 0
  if (over >= half) return over * slope
  const t = over + half
  return (slope * t * t) / (4 * half)
}

/// Crossovers low to high, the order the DSP runs them in
/// (`compresser::effective_crossovers` sorts the same way).
export function sortedCrossovers(params: CompressorParams): number[] {
  return [...params.crossoverHz].sort((a, b) => a - b)
}

/// The range a crossover may be dragged over without passing a neighbour.
export function crossoverBounds(sorted: number[], index: number): [number, number] {
  const below = index > 0 ? sorted[index - 1]! * MIN_CROSSOVER_RATIO : MIN_CROSSOVER_HZ
  const above =
    index < sorted.length - 1 ? sorted[index + 1]! / MIN_CROSSOVER_RATIO : MAX_CROSSOVER_HZ
  return [Math.max(MIN_CROSSOVER_HZ, below), Math.min(MAX_CROSSOVER_HZ, above)]
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

/// Knob travel on a log scale between `min` and `max` — time constants and
/// ratios are heard in proportion, not in steps.
export function logTravel(min: number, max: number) {
  return {
    toProgress: (value: number) => Math.log(clamp(value, min, max) / min) / Math.log(max / min),
    fromProgress: (progress: number) => min * Math.pow(max / min, clamp(progress, 0, 1)),
  }
}

export function formatHz(hz: number): string {
  if (hz >= 10_000) return `${(hz / 1000).toFixed(1)}k`
  if (hz >= 1000) return `${(hz / 1000).toFixed(2)}k`
  return `${Math.round(hz)}`
}

export function formatDb(db: number): string {
  const rounded = Math.round(db * 10) / 10
  return `${rounded > 0 ? '+' : ''}${rounded.toFixed(1)}`
}

export function formatThreshold(db: number): string {
  return (Math.round(db * 10) / 10).toFixed(1)
}

export function formatRatio(ratio: number): string {
  return ratio >= 10 ? ratio.toFixed(0) : ratio.toFixed(1)
}

export function formatMs(ms: number): string {
  if (ms < 10) return ms.toFixed(1)
  return `${Math.round(ms)}`
}

export function formatPercent(value: number): string {
  return `${Math.round(value)}`
}

export function formatSidechain(hz: number): string {
  return hz <= SIDECHAIN_OFF_HZ ? 'Off' : formatHz(hz)
}
