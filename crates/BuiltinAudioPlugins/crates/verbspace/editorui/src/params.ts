/**
 * Editor-side mirror of VerbSpace's parameter contract.
 *
 * Rust owns the authority: `src/ipc.rs` defines the ids and their ranges, and
 * clamps every incoming value again. This table exists so the UI can lay out a
 * control, choose a taper, and format a readout — never to decide what a value
 * means. `params.test.ts` pins the ids and ranges against the Rust source.
 */

export type ParamId =
  | 'predelayMs'
  | 'size'
  | 'decaySec'
  | 'diffusion'
  | 'damping'
  | 'bassMult'
  | 'modDepth'
  | 'modRateHz'
  | 'lowCutHz'
  | 'highCutHz'
  | 'width'
  | 'mix'
  | 'outputDb'

/**
 * How the 0..1 control travel maps onto the value range.
 *
 * - `lin` — uniform.
 * - `exp` — constant ratio per unit travel; the only sane choice for a
 *   frequency or a rate, where the useful resolution is at the bottom.
 * - a number — `value = min + (max - min) * t ** exponent`, for ranges that
 *   want more resolution low down without being fully logarithmic.
 */
type Taper = 'lin' | 'exp' | number

export type ParamSpec = {
  id: ParamId
  label: string
  min: number
  max: number
  default: number
  unit: string
  taper: Taper
  /** Readout precision in decimal places. */
  digits: number
  /** Arrow-key / wheel step, in value units. */
  step: number
  /**
   * Value the dial fills *out from*. Defaults to `min`. Set it for controls
   * with a real neutral point — a bass multiplier of 1× or a width of 100 %
   * change nothing, and reading that as a half-full dial is a lie. Note this
   * is the neutral value, not the default: VerbSpace ships with a little more
   * than neutral on both.
   */
  origin?: number
}

const spec = (
  id: ParamId,
  label: string,
  min: number,
  max: number,
  def: number,
  unit: string,
  taper: Taper,
  digits: number,
  step: number,
  origin?: number,
): ParamSpec => ({
  id,
  label,
  min,
  max,
  default: def,
  unit,
  taper,
  digits,
  step,
  origin,
})

export const PARAMS: Record<ParamId, ParamSpec> = {
  predelayMs: spec('predelayMs', 'Pre-Delay', 0, 500, 20, 'ms', 2, 1, 1),
  size: spec('size', 'Size', 10, 100, 60, '%', 'lin', 0, 1),
  decaySec: spec('decaySec', 'Decay', 0.1, 20, 2.4, 's', 2.2, 2, 0.1),
  diffusion: spec('diffusion', 'Diffusion', 0, 100, 72, '%', 'lin', 0, 1),
  damping: spec('damping', 'Damping', 0, 100, 45, '%', 'lin', 0, 1),
  bassMult: spec('bassMult', 'Bass', 0.2, 2, 1.1, '×', 'lin', 2, 0.05, 1),
  modDepth: spec('modDepth', 'Depth', 0, 100, 25, '%', 'lin', 0, 1),
  modRateHz: spec('modRateHz', 'Rate', 0.05, 5, 0.6, 'Hz', 'exp', 2, 0.05),
  lowCutHz: spec('lowCutHz', 'Low Cut', 20, 1000, 90, 'Hz', 'exp', 0, 1),
  highCutHz: spec('highCutHz', 'High Cut', 1000, 20000, 9500, 'Hz', 'exp', 0, 10),
  width: spec('width', 'Width', 0, 200, 110, '%', 'lin', 0, 1, 100),
  mix: spec('mix', 'Mix', 0, 100, 28, '%', 'lin', 0, 1),
  outputDb: spec('outputDb', 'Output', -24, 12, 0, 'dB', 'lin', 1, 0.5, 0),
}

export const MODES = ['room', 'chamber', 'hall', 'plate', 'ambience'] as const
export type Mode = (typeof MODES)[number]

export const MODE_LABELS: Record<Mode, string> = {
  room: 'Room',
  chamber: 'Chamber',
  hall: 'Hall',
  plate: 'Plate',
  ambience: 'Ambience',
}

/**
 * One plain line per space, saying what the mode changes in the tank: how
 * long its lines are (`ReverbMode::line_scale`) and how much diffusion it
 * starts from (`ReverbMode::diffusion_bias`). `model.test.ts` holds each
 * claim against those constants.
 */
export const MODE_HINTS: Record<Mode, string> = {
  room: 'Short tank, light diffusion',
  chamber: 'Medium tank, smoother build',
  hall: 'Largest tank, slowest build',
  plate: 'Small tank, densest from the start',
  ambience: 'Smallest tank, sparsest',
}

/** Wire value for `mode`; the index in [`MODES`] is the contract. */
export function modeToWire(mode: Mode): number {
  const index = MODES.indexOf(mode)
  return index < 0 ? MODES.indexOf('hall') : index
}

export function modeFromWire(value: number): Mode {
  return MODES[Math.round(value)] ?? 'hall'
}

export function clamp(value: number, min: number, max: number): number {
  return value < min ? min : value > max ? max : value
}

/** Value -> 0..1 control travel. */
export function toNorm(spec: ParamSpec, value: number): number {
  const v = clamp(value, spec.min, spec.max)
  if (spec.taper === 'exp') {
    const lo = Math.log(Math.max(spec.min, 1e-6))
    return (Math.log(Math.max(v, 1e-6)) - lo) / (Math.log(spec.max) - lo)
  }
  const t = (v - spec.min) / (spec.max - spec.min)
  return spec.taper === 'lin' ? t : Math.pow(t, 1 / spec.taper)
}

/** 0..1 control travel -> value. */
export function fromNorm(spec: ParamSpec, norm: number): number {
  const t = clamp(norm, 0, 1)
  if (spec.taper === 'exp') {
    const lo = Math.log(Math.max(spec.min, 1e-6))
    return Math.exp(lo + t * (Math.log(spec.max) - lo))
  }
  const shaped = spec.taper === 'lin' ? t : Math.pow(t, spec.taper)
  return spec.min + shaped * (spec.max - spec.min)
}

/** Unit shown next to the readout. VerbSpace's units never change with the
 *  value; the signature matches EchoSpace's so the two share one dial. */
export function unitFor(spec: ParamSpec, _value: number): string {
  return spec.unit
}

/** Readout text. Frequencies switch to kHz where the four-digit form stops
 * scanning quickly. */
export function format(spec: ParamSpec, value: number): string {
  const v = clamp(value, spec.min, spec.max)
  if (spec.unit === 'Hz' && v >= 1000) {
    return `${(v / 1000).toFixed(v >= 10000 ? 1 : 2)}k`
  }
  if (spec.unit === 'dB' && v > 0) {
    return `+${v.toFixed(spec.digits)}`
  }
  return v.toFixed(spec.digits)
}
