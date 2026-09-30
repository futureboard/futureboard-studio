/** Small numeric helpers shared by the controls and displays. */

export function clamp(value: number, min: number, max: number) {
  return value < min ? min : value > max ? max : value
}

export function dbToLinear(db: number) {
  return Math.pow(10, db / 20)
}

export function linearToDb(linear: number) {
  return 20 * Math.log10(Math.max(linear, 1e-6))
}

/// Knob travel on a log scale between `min` and `max` — times are heard in
/// proportion, not in steps.
export function logTravel(min: number, max: number) {
  return {
    toProgress: (value: number) => Math.log(clamp(value, min, max) / min) / Math.log(max / min),
    fromProgress: (progress: number) => min * Math.pow(max / min, clamp(progress, 0, 1)),
  }
}

/// Signed decibels with one decimal: `+1.5`, `0.0`, `-3.0`.
export function formatDb(db: number): string {
  const rounded = Math.round(db * 10) / 10
  return `${rounded > 0 ? '+' : ''}${rounded.toFixed(1)}`
}

/// A bipolar percentage: `+40`, `0`, `-25`.
export function formatSigned(value: number): string {
  const rounded = Math.round(value)
  return `${rounded > 0 ? '+' : ''}${rounded}`
}

export function formatInt(value: number): string {
  return `${Math.round(value)}`
}

export function formatMs(ms: number): string {
  return ms < 10 ? ms.toFixed(1) : `${Math.round(ms)}`
}
