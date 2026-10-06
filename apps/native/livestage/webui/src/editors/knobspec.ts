// A knob's mapping and readout: a port of the native editors' `KnobSpec`
// (crates/SphereUIComponents/src/components/fx_model.rs), so a knob turns the
// same way and reads the same in the web UI as in Studio.

export type Taper = 'linear' | 'log' | 'square'

export type Unit =
  | 'ms'
  | 'sec'
  | 'hz'
  | 'percent'
  | 'db'
  | 'times'
  /** A note division, by its index into DIVISION_LABELS. */
  | 'division'
  | 'semitones'
  | 'cents'
  /** A depth either side, in cents. */
  | 'centsDepth'
  | 'us'
  /** A compression ratio, n:1. */
  | 'ratio'
  | 'plain'
  /** A sidechain filter in Hz, off at or below `cutOff`. */
  | 'cutHz'

export interface KnobSpec {
  id: string
  label: string
  min: number
  max: number
  taper: Taper
  unit: Unit
  /** Drawn from `centre` outward rather than from the bottom. */
  bipolar?: boolean
  /** The value a bipolar knob's arc starts from. */
  centre?: number
  /** For unit 'cutHz': at or below this, the filter is off. */
  cutOff?: number
}

export function spec(id: string, label: string, min: number, max: number, taper: Taper, unit: Unit): KnobSpec {
  return { id, label, min, max, taper, unit }
}

export function bipolar(s: KnobSpec, centre = 0): KnobSpec {
  return { ...s, bipolar: true, centre }
}

/** echospace::DIVISION_LABELS */
export const DIVISION_LABELS = [
  '1/32T', '1/32', '1/16T', '1/32.', '1/16', '1/8T', '1/16.', '1/8', '1/4T', '1/8.', '1/4',
  '1/2T', '1/4.', '1/2', '1/1T', '1/2.', '1/1', '1/1.',
]

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v))

/** The knob's position, 0–1, for `value`. */
export function fraction(s: KnobSpec, value: number): number {
  const v = clamp(value, s.min, s.max)
  switch (s.taper) {
    case 'linear':
      return (v - s.min) / (s.max - s.min)
    case 'log':
      return Math.log(v / s.min) / Math.log(s.max / s.min)
    case 'square':
      return Math.sqrt((v - s.min) / (s.max - s.min))
  }
}

/** The value at knob position `f`, rounded to what the readout shows. */
export function valueAt(s: KnobSpec, f: number): number {
  const t = clamp(f, 0, 1)
  let raw: number
  switch (s.taper) {
    case 'linear':
      raw = s.min + t * (s.max - s.min)
      break
    case 'log':
      raw = s.min * Math.pow(s.max / s.min, t)
      break
    case 'square':
      raw = s.min + t * t * (s.max - s.min)
      break
  }
  return roundFor(s.unit, raw)
}

/** The span the knob widget runs over: 0–1 through the taper for a unipolar
 *  knob; the raw range shifted so `centre` is zero for a bipolar one. */
export function knobRange(s: KnobSpec): [number, number] {
  const c = s.centre ?? 0
  return s.bipolar ? [s.min - c, s.max - c] : [0, 1]
}

export function toKnob(s: KnobSpec, value: number): number {
  return s.bipolar ? clamp(value, s.min, s.max) - (s.centre ?? 0) : fraction(s, value)
}

export function fromKnob(s: KnobSpec, units: number): number {
  return s.bipolar ? roundFor(s.unit, clamp(units + (s.centre ?? 0), s.min, s.max)) : valueAt(s, units)
}

/** Rounds to the step the readout shows, so no value hides behind it. */
export function roundFor(unit: Unit, value: number): number {
  let step: number
  switch (unit) {
    case 'ms':
      step = value < 1 ? 0.01 : value < 10 ? 0.1 : 1
      break
    case 'sec':
      step = value < 10 ? 0.01 : 0.1
      break
    case 'hz':
      step = value < 1 ? 0.01 : value < 10 ? 0.1 : value < 1000 ? 1 : 10
      break
    case 'percent':
      step = 1
      break
    case 'db':
      step = 0.1
      break
    case 'times':
      step = 0.01
      break
    case 'ratio':
      step = value < 10 ? 0.1 : 0.5
      break
    default:
      step = 1
  }
  return Math.round(value / step) * step
}

// As Rust's `{:+.1}`: an ASCII sign either way.
const signed = (v: number, digits: number) => `${v >= 0 ? '+' : '-'}${Math.abs(v).toFixed(digits)}`

export function formatValue(unit: Unit, value: number, cutOff = 0): string {
  switch (unit) {
    case 'ms':
      if (value >= 1000) return `${(value / 1000).toFixed(2)} s`
      if (value < 1) return `${value.toFixed(2)} ms`
      if (value < 10) return `${value.toFixed(1)} ms`
      return `${value.toFixed(0)} ms`
    case 'sec':
      if (!Number.isFinite(value)) return '∞'
      return value < 10 ? `${value.toFixed(2)} s` : `${value.toFixed(1)} s`
    case 'hz':
      if (value < 1) return `${value.toFixed(2)} Hz`
      if (value < 10) return `${value.toFixed(1)} Hz`
      if (value < 1000) return `${value.toFixed(0)} Hz`
      return `${(value / 1000).toFixed(1)} kHz`
    case 'percent':
      return `${value.toFixed(0)} %`
    case 'db':
      return Math.abs(value) < 0.05 ? '0.0 dB' : `${signed(value, 1)} dB`
    case 'times':
      return `${value.toFixed(2)}×`
    case 'semitones':
      return Math.abs(value) < 0.5 ? '0 st' : `${signed(value, 0)} st`
    case 'cents':
      return Math.abs(value) < 0.5 ? '0 ¢' : `${signed(value, 0)} ¢`
    case 'centsDepth':
      return `±${value.toFixed(0)} ¢`
    case 'us':
      return `${value.toFixed(0)} µs`
    case 'ratio':
      return value < 10 ? `${value.toFixed(1)}:1` : `${value.toFixed(0)}:1`
    case 'plain':
      return value.toFixed(0)
    case 'cutHz':
      return value <= cutOff ? 'Off' : formatValue('hz', value)
    case 'division':
      return DIVISION_LABELS[clamp(Math.round(value), 0, DIVISION_LABELS.length - 1)]
  }
}

export function readout(s: KnobSpec, value: number): string {
  return formatValue(s.unit, value, s.cutOff ?? 0)
}
