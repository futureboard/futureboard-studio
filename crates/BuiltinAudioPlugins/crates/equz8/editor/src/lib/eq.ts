import { area, curveMonotoneX, line, scaleLinear, scaleLog } from 'd3'
import type { Band, FilterType } from '../bridge'

export const MIN_FREQ = 20
export const MAX_FREQ = 20_000
export const GAIN_RANGE = 18
export const MIN_Q = 0.1
export const MAX_Q = 12
export const OUTPUT_MIN_DB = -24
export const OUTPUT_MAX_DB = 12
/// Rate assumed before the host reports the real one, and the fallback in a
/// plain browser where no host is bound.
export const DEFAULT_SAMPLE_RATE = 48_000

/// Highest centre frequency a filter can be tuned to at `sampleRate`.
///
/// Mirrors `builtin_dsp_core::MAX_FILTER_FREQUENCY_RATIO`. The DSP clamps to
/// this, so the curve has to as well or it would draw a band where the audio
/// cannot actually put one.
export const MAX_FILTER_FREQUENCY_RATIO = 0.49

export function maxFilterFrequency(sampleRate: number) {
  return sampleRate * MAX_FILTER_FREQUENCY_RATIO
}

/// The graph's two axes, as single shared d3 scales.
///
/// Everything drawn on the response graph — grid, axis labels, band curves, the
/// summed curve, the draggable nodes, the cursor readout and the WebGL spectrum
/// underneath — goes through these. One transform, one place to change it, and
/// no way for the overlay to drift out of register with the curve it sits under.
///
/// d3 scales are mutable, and these are called per point while tracing a curve,
/// so the range is rewritten only when the measured size actually changes
/// rather than rebuilding a scale per call.
const freqScale = scaleLog().domain([MIN_FREQ, MAX_FREQ]).range([0, 1])
const gainScale = scaleLinear().domain([GAIN_RANGE, -GAIN_RANGE]).range([0, 1])

let freqWidth = -1
let gainHeight = -1

function xScale(width: number) {
  if (freqWidth !== width) {
    freqScale.range([0, width])
    freqWidth = width
  }
  return freqScale
}

function yScale(height: number) {
  if (gainHeight !== height) {
    gainScale.range([0, height])
    gainHeight = height
  }
  return gainScale
}

export type FilterKind = {
  type: FilterType
  label: string
  short: string
  wire: number
  glyph: string
}

export const FILTER_KINDS: FilterKind[] = [
  {
    type: 'highpass',
    label: 'High pass',
    short: 'HP',
    wire: 0,
    glyph: 'M3 17c8 0 6-12 15-12h11',
  },
  {
    type: 'lowshelf',
    label: 'Low shelf',
    short: 'LS',
    wire: 1,
    glyph: 'M3 15h8c7 0 6-10 13-10h5',
  },
  {
    type: 'bell',
    label: 'Bell',
    short: 'BELL',
    wire: 2,
    glyph: 'M3 15c7 0 7-10 13-10s6 10 13 10',
  },
  {
    type: 'notch',
    label: 'Notch',
    short: 'NOTCH',
    wire: 3,
    glyph: 'M3 5c7 0 7 10 13 10s6-10 13-10',
  },
  {
    type: 'highshelf',
    label: 'High shelf',
    short: 'HS',
    wire: 4,
    glyph: 'M3 5h5c7 0 6 10 13 10h8',
  },
  {
    type: 'lowpass',
    label: 'Low pass',
    short: 'LP',
    wire: 5,
    glyph: 'M3 5h10c9 0 7 12 16 12',
  },
]

/// Pro-Q-style band identity — saturated spectrum markers so each handle reads
/// instantly against the dark analyser floor (cool lows → warm highs).
export const BAND_COLORS = [
  '#3ec7ff',
  '#2ad4c1',
  '#5adf7a',
  '#d4d04a',
  '#ff9a3c',
  '#ff6b6b',
  '#c084fc',
  '#7aa2ff',
] as const

export function filterKind(type: FilterType): FilterKind {
  return FILTER_KINDS.find((kind) => kind.type === type) ?? FILTER_KINDS[2]!
}

export function bandHasGain(type: FilterType) {
  return type === 'bell' || type === 'lowshelf' || type === 'highshelf'
}

export function clamp(value: number, min: number, max: number) {
  return Math.min(max, Math.max(min, value))
}

export function frequencyToX(frequency: number, width: number) {
  return xScale(width)(clamp(frequency, MIN_FREQ, MAX_FREQ))
}

export function xToFrequency(x: number, width: number) {
  return xScale(width).invert(clamp(x, 0, width))
}

export function gainToY(gain: number, height: number) {
  return yScale(height)(gain)
}

export function yToGain(y: number, height: number) {
  return yScale(height).invert(clamp(y, 0, height))
}

/// Position along the frequency axis as `0..1`, for controls that lay out in
/// their own space (the frequency knob) but must still land where the graph
/// puts the same value.
export function freqToProgress(frequency: number) {
  return frequencyToX(frequency, 1)
}

export function progressToFreq(progress: number) {
  return xToFrequency(clamp(progress, 0, 1), 1)
}

/// Q is perceptually *multiplicative*: 0.5 -> 1 is the same musical step as
/// 4 -> 8. Laying 0.1..12 out linearly therefore crams the entire useful range
/// into the bottom fifth of the control — most of the travel is spent between
/// 3 and 12, where a whole unit of Q is barely audible, while the region people
/// actually work in gets almost no resolution. That is what made the control
/// feel like it needed a long drag before anything happened.
///
/// A log mapping gives every octave of Q the same distance, matching how the
/// frequency axis already behaves.
const qScale = scaleLog().domain([MIN_Q, MAX_Q]).range([0, 1])

export function qToProgress(q: number) {
  return clamp(qScale(clamp(q, MIN_Q, MAX_Q)), 0, 1)
}

export function progressToQ(progress: number) {
  return clamp(qScale.invert(clamp(progress, 0, 1)), MIN_Q, MAX_Q)
}

/// Scale Q by a ratio, for wheel/scroll gestures.
///
/// Multiplying keeps each notch the same *perceived* step anywhere in the
/// range. The previous additive step of 0.12 meant a notch more than doubled Q
/// at the bottom of the range while moving it by 1% at the top, so widening a
/// narrow band took a handful of notches and narrowing a wide one took dozens.
export function scaleQ(q: number, ratio: number) {
  return clamp(clamp(q, MIN_Q, MAX_Q) * ratio, MIN_Q, MAX_Q)
}

/// Log ticks straight from the scale — every multiple inside each decade, so a
/// zoomed-out grid still reads as logarithmic instead of evenly spaced.
export const GRID_FREQUENCIES = freqScale
  .ticks()
  .filter((value) => value >= MIN_FREQ && value <= MAX_FREQ)

export const LABELLED_FREQUENCIES = [30, 100, 300, 1_000, 3_000, 10_000]
export const GRID_GAINS = [12, 6, 0, -6, -12]

export function formatFrequency(value: number) {
  if (value >= 10_000) return `${(value / 1000).toFixed(1).replace(/\.0$/, '')}k`
  if (value >= 1_000) return `${(value / 1000).toFixed(2).replace(/\.?0+$/, '')}k`
  return `${Math.round(value)}`
}

export function formatGain(value: number) {
  const normalized = Math.abs(value) < 0.05 ? 0 : value
  return `${normalized > 0 ? '+' : ''}${normalized.toFixed(1)}`
}

export function formatQ(value: number) {
  return value.toFixed(2)
}

type Coefficients = {
  b0: number
  b1: number
  b2: number
  a1: number
  a2: number
}

export function bandCoefficients(
  band: Band,
  sampleRate: number,
): Coefficients {
  // Clamped the same way the DSP clamps it, so the drawn curve cannot show a
  // band sitting somewhere the filter is not able to go.
  const f0 = Math.min(
    clamp(band.freq, MIN_FREQ, MAX_FREQ),
    maxFilterFrequency(sampleRate),
  )
  const w0 = (2 * Math.PI * f0) / sampleRate
  const cos = Math.cos(w0)
  const sin = Math.sin(w0)
  const alpha = sin / (2 * clamp(band.q, MIN_Q, MAX_Q))
  const a = Math.pow(10, band.gainDb / 40)
  let b0: number
  let b1: number
  let b2: number
  let a0: number
  let a1: number
  let a2: number

  switch (band.bandType) {
    case 'highpass':
      b0 = (1 + cos) / 2
      b1 = -(1 + cos)
      b2 = (1 + cos) / 2
      a0 = 1 + alpha
      a1 = -2 * cos
      a2 = 1 - alpha
      break
    case 'lowpass':
      b0 = (1 - cos) / 2
      b1 = 1 - cos
      b2 = (1 - cos) / 2
      a0 = 1 + alpha
      a1 = -2 * cos
      a2 = 1 - alpha
      break
    case 'notch':
      b0 = 1
      b1 = -2 * cos
      b2 = 1
      a0 = 1 + alpha
      a1 = -2 * cos
      a2 = 1 - alpha
      break
    case 'lowshelf': {
      const root = 2 * Math.sqrt(a) * alpha
      b0 = a * (a + 1 - (a - 1) * cos + root)
      b1 = 2 * a * (a - 1 - (a + 1) * cos)
      b2 = a * (a + 1 - (a - 1) * cos - root)
      a0 = a + 1 + (a - 1) * cos + root
      a1 = -2 * (a - 1 + (a + 1) * cos)
      a2 = a + 1 + (a - 1) * cos - root
      break
    }
    case 'highshelf': {
      const root = 2 * Math.sqrt(a) * alpha
      b0 = a * (a + 1 + (a - 1) * cos + root)
      b1 = -2 * a * (a - 1 + (a + 1) * cos)
      b2 = a * (a + 1 + (a - 1) * cos - root)
      a0 = a + 1 - (a - 1) * cos + root
      a1 = 2 * (a - 1 - (a + 1) * cos)
      a2 = a + 1 - (a - 1) * cos - root
      break
    }
    default:
      b0 = 1 + alpha * a
      b1 = -2 * cos
      b2 = 1 - alpha * a
      a0 = 1 + alpha / a
      a1 = -2 * cos
      a2 = 1 - alpha / a
  }

  return { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
}

function magnitudeDb(
  coeff: Coefficients,
  frequency: number,
  sampleRate: number,
) {
  const w = (2 * Math.PI * frequency) / sampleRate
  const c1 = Math.cos(w)
  const s1 = Math.sin(w)
  const c2 = Math.cos(2 * w)
  const s2 = Math.sin(2 * w)
  const nr = coeff.b0 + coeff.b1 * c1 + coeff.b2 * c2
  const ni = -coeff.b1 * s1 - coeff.b2 * s2
  const dr = 1 + coeff.a1 * c1 + coeff.a2 * c2
  const di = -coeff.a1 * s1 - coeff.a2 * s2
  const magnitude = Math.sqrt((nr * nr + ni * ni) / (dr * dr + di * di))
  return 20 * Math.log10(Math.max(magnitude, 1e-6))
}

function sampleCount(width: number) {
  return Math.round(clamp(width / 3, 140, 420))
}

/// Monotone interpolation, not a basis/cardinal spline: those overshoot at a
/// steep filter edge, which on a response graph would draw resonance the filter
/// does not have. Monotone never introduces an extremum the samples lack, so
/// the drawn curve cannot claim more than the maths does.
const curveBuilder = line<[number, number]>()
  .x((point) => point[0])
  .y((point) => point[1])
  .curve(curveMonotoneX)

function sampleCurvePoints(
  width: number,
  height: number,
  dbAt: (frequency: number) => number,
) {
  const samples = sampleCount(width)
  const points: [number, number][] = new Array(samples + 1)
  for (let index = 0; index <= samples; index += 1) {
    const x = (index / samples) * width
    const db = clamp(dbAt(xToFrequency(x, width)), -GAIN_RANGE - 6, GAIN_RANGE + 6)
    points[index] = [x, gainToY(db, height)]
  }
  return points
}

function tracePath(
  width: number,
  height: number,
  dbAt: (frequency: number) => number,
) {
  return curveBuilder(sampleCurvePoints(width, height, dbAt)) ?? ''
}

/// Filled response under the sum curve (Pro-Q area shade), closed on the 0 dB
/// line via d3.area so the SVG fill stays in register with the stroked path.
export function sumCurveAreaPath(
  bands: Band[],
  width: number,
  height: number,
  sampleRate: number,
) {
  const active = bands
    .filter((band) => band.active)
    .map((band) => bandCoefficients(band, sampleRate))
  const zeroY = gainToY(0, height)
  const points = sampleCurvePoints(width, height, (frequency) =>
    active.reduce(
      (sum, coeff) => sum + magnitudeDb(coeff, frequency, sampleRate),
      0,
    ),
  )
  const areaBuilder = area<[number, number]>()
    .x((point) => point[0])
    .y0(zeroY)
    .y1((point) => point[1])
    .curve(curveMonotoneX)
  return areaBuilder(points) ?? ''
}

export function sumCurvePath(
  bands: Band[],
  width: number,
  height: number,
  sampleRate: number,
) {
  const active = bands
    .filter((band) => band.active)
    .map((band) => bandCoefficients(band, sampleRate))
  return tracePath(width, height, (frequency) =>
    active.reduce(
      (sum, coeff) => sum + magnitudeDb(coeff, frequency, sampleRate),
      0,
    ),
  )
}

/// One band's response as an area closed on the 0 dB line — the shaded shape
/// under the selected band, in register with its stroked curve.
export function bandCurveAreaPath(
  band: Band,
  width: number,
  height: number,
  sampleRate: number,
) {
  const coeff = bandCoefficients(band, sampleRate)
  const zeroY = gainToY(0, height)
  const points = sampleCurvePoints(width, height, (frequency) =>
    magnitudeDb(coeff, frequency, sampleRate),
  )
  const areaBuilder = area<[number, number]>()
    .x((point) => point[0])
    .y0(zeroY)
    .y1((point) => point[1])
    .curve(curveMonotoneX)
  return areaBuilder(points) ?? ''
}

export function bandCurvePath(
  band: Band,
  width: number,
  height: number,
  sampleRate: number,
) {
  const coeff = bandCoefficients(band, sampleRate)
  return tracePath(width, height, (frequency) =>
    magnitudeDb(coeff, frequency, sampleRate),
  )
}

export function sumDbAt(bands: Band[], frequency: number, sampleRate: number) {
  return bands
    .filter((band) => band.active)
    .reduce(
      (sum, band) =>
        sum + magnitudeDb(bandCoefficients(band, sampleRate), frequency, sampleRate),
      0,
    )
}
