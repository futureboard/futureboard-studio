// Editors for EchoSpace and VerbSpace: a port of the native time-effect
// editor (components/fx_model.rs, fx_panel.rs, fx_window.rs).
//
// A toolbar (mode, sync/link, freeze), two displays drawn from the params
// through the DSP crates' own models (echospace::echo_pattern and
// tone_response_db, verbspace::decay_profile and wet_filter_response_db,
// ported below), and a row of knob cards. Neither effect publishes levels,
// so nothing here reads telemetry.

import { useEffect, useRef } from 'react'
import type { CSSProperties, ReactNode } from 'react'
import { act } from '../store.ts'
import type { Editor, EditorComponent } from './kit.tsx'
import { Card, Check, Choice, EditorShell, KNOB, KitKnob, LiveCanvas, ParamChoice, Row } from './kit.tsx'
import type { KnobSpec, Taper, Unit } from './knobspec.ts'
import { DIVISION_LABELS, bipolar, spec } from './knobspec.ts'
import type { Ctx } from './paint.ts'
import { alpha, area, colors, freqAtFraction, freqFraction, line, rect } from './paint.ts'
import './fx.css'

type Kind = 'verb' | 'echo'

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v))
const log10 = Math.log10

// ── Knobs (fx_model::knob) ──────────────────────────────────────────────

const KNOBS: Record<Kind, Record<string, [string, Taper, Unit]>> = {
  verb: {
    predelayMs: ['Pre-Delay', 'square', 'ms'],
    size: ['Size', 'linear', 'percent'],
    decaySec: ['Decay', 'log', 'sec'],
    diffusion: ['Diffusion', 'linear', 'percent'],
    damping: ['Damping', 'linear', 'percent'],
    bassMult: ['Bass', 'log', 'times'],
    bassFreqHz: ['Bass Freq', 'log', 'hz'],
    dampFreqHz: ['Damp Freq', 'log', 'hz'],
    earlyLate: ['Early/Late', 'linear', 'percent'],
    modDepth: ['Depth', 'linear', 'percent'],
    modRateHz: ['Rate', 'log', 'hz'],
  },
  echo: {
    timeMsL: ['Time L', 'log', 'ms'],
    timeMsR: ['Time R', 'log', 'ms'],
    divisionL: ['Note L', 'linear', 'division'],
    divisionR: ['Note R', 'linear', 'division'],
    feedback: ['Feedback', 'linear', 'percent'],
    crossFeedback: ['Cross', 'linear', 'percent'],
    saturation: ['Drive', 'linear', 'percent'],
    modDepth: ['Wow', 'linear', 'percent'],
    modRateHz: ['Rate', 'log', 'hz'],
    duck: ['Duck', 'linear', 'percent'],
    diffusion: ['Diffusion', 'linear', 'percent'],
  },
}

const SHARED_KNOBS: Record<string, [string, Taper, Unit]> = {
  lowCutHz: ['Low Cut', 'log', 'hz'],
  highCutHz: ['High Cut', 'log', 'hz'],
  width: ['Width', 'linear', 'percent'],
  mix: ['Mix', 'linear', 'percent'],
  outputDb: ['Output', 'linear', 'db'],
}

/** The knob for `id`; its range from the DSP's descriptor, as natively. */
function knobOf(editor: Editor, kind: Kind, id: string): KnobSpec | null {
  const range = editor.effect.params.find((p) => p.id === id)
  const row = KNOBS[kind][id] ?? SHARED_KNOBS[id]
  if (!range || !row) return null
  const knob = spec(id, row[0], range.min, range.max, row[1], row[2])
  // Output trims around unity, width around an untouched image.
  if (id === 'outputDb') return bipolar(knob, 0)
  if (id === 'width') return bipolar(knob, 100)
  return knob
}

/** A knob, or — when the mode leaves it nothing to do — greyed with `why`
 *  in place of its value. */
function FxKnob(props: { editor: Editor; kind: Kind; id: string; why?: string }) {
  const knob = knobOf(props.editor, props.kind, props.id)
  if (!knob) return null
  if (props.why) return <IdleKnob label={knob.label} why={props.why} />
  return <KitKnob editor={props.editor} spec={knob} />
}

/** The native greyed knob: parked at its start, the reason as its readout.
 *  Not a control — the mode decides it, so it takes no input. */
function IdleKnob(props: { label: string; why: string }) {
  const size = KNOB
  const c = size / 2
  const r = size / 2 - 2.5
  const angle = (f: number) => (-135 + 270 * f) * (Math.PI / 180)
  const point = (f: number, radius = r) => [c + radius * Math.sin(angle(f)), c - radius * Math.cos(angle(f))]
  const [x0, y0] = point(0)
  const [x1, y1] = point(1)
  const [px0, py0] = point(0, r * 0.28)
  const [px1, py1] = point(0, r * 0.62)
  return (
    <div className="pe-knob fx-idle" title={`${props.label}: ${props.why}`} aria-disabled>
      <div className="knob">
        <svg width={size} height={size} aria-hidden>
          <path className="knob-track" d={`M ${x0} ${y0} A ${r} ${r} 0 1 1 ${x1} ${y1}`} />
          <circle className="knob-body" cx={c} cy={c} r={r * 0.72} />
          <line className="knob-pointer" x1={px0} y1={py0} x2={px1} y2={py1} />
        </svg>
        <span className="knob-caption">{props.label}</span>
        <span className="knob-text">{props.why}</span>
      </div>
    </div>
  )
}

/** A card of knobs: a share of the row by how many it holds, never
 *  narrower than they are (fx_panel::section). */
function Section(props: { title: string; count: number; children: ReactNode }) {
  return (
    <Card
      title={props.title}
      grow={props.count}
      className="fx-card"
      style={{ '--fx-knobs': props.count } as CSSProperties}
    >
      <div className="pe-knobs">{props.children}</div>
    </Card>
  )
}

// ── Shared filter maths (builtin_dsp_core + the biquad crate) ───────────

interface Coefficients {
  b0: number
  b1: number
  b2: number
  a1: number
  a2: number
}

/** make_eq_coefficients for the two cut shapes: the RBJ low/high-pass the
 *  biquad crate builds, its corner clamped under 0.49 × the rate. */
function cutCoefficients(kind: 'lowpass' | 'highpass', hz: number, sampleRate: number): Coefficients | null {
  const fs = Math.max(1, sampleRate)
  const f0 = clamp(hz, 10, fs * 0.49)
  const q = clamp(0.707, 0.1, 12)
  const normalized = (2 * f0) / fs
  if (normalized >= 1 || normalized < 0) return null
  const omega = Math.PI * normalized
  const cos = Math.cos(omega)
  const a = Math.sin(omega) / (2 * q)
  const a0 = 1 + a
  const [b0, b1, b2] = kind === 'lowpass' ? [(1 - cos) / 2, 1 - cos, (1 - cos) / 2] : [(1 + cos) / 2, -(1 + cos), (1 + cos) / 2]
  return { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: (-2 * cos) / a0, a2: (1 - a) / a0 }
}

/** biquad_response_db */
function responseDb(c: Coefficients, hz: number, sampleRate: number): number {
  const w = (2 * Math.PI * hz) / Math.max(1, sampleRate)
  const [cos1, sin1, cos2, sin2] = [Math.cos(w), Math.sin(w), Math.cos(2 * w), Math.sin(2 * w)]
  const numRe = c.b0 + c.b1 * cos1 + c.b2 * cos2
  const numIm = -(c.b1 * sin1 + c.b2 * sin2)
  const denRe = 1 + c.a1 * cos1 + c.a2 * cos2
  const denIm = -(c.a1 * sin1 + c.a2 * sin2)
  const power = (numRe * numRe + numIm * numIm) / Math.max(1e-30, denRe * denRe + denIm * denIm)
  return 10 * log10(Math.max(1e-30, power))
}

/** Both effects' cut pair, as their `cut_coefficients`: what one pass
 *  through EchoSpace's tone stage, or VerbSpace's wet path, does at `hz`. */
function cutsDb(lowCutHz: number, highCutHz: number, hz: number, sampleRate: number): number {
  const guard = sampleRate * 0.45
  return [
    cutCoefficients('highpass', clamp(lowCutHz, 20, guard), sampleRate),
    cutCoefficients('lowpass', clamp(highCutHz, 200, guard), sampleRate),
  ].reduce((sum, c) => (c ? sum + responseDb(c, hz, sampleRate) : sum), 0)
}

const dbToLinear = (db: number) => Math.pow(10, db / 20)

/** Points across a frequency curve (fx_window::CURVE_POINTS). */
const CURVE_POINTS = 96
const curveHz = (i: number) => freqAtFraction(i / (CURVE_POINTS - 1))

// ── EchoSpace's model (echospace/src/lib.rs) ────────────────────────────

const ECHO_MODES = [
  [0, 'Stereo'],
  [1, 'Ping-Pong'],
  [2, 'Mono'],
] as const
const STEREO = 0
const PINGPONG = 1
const MONO = 2

/** DelayMode::from_wire */
const delayMode = (v: number) => {
  const m = Math.round(v)
  return m === 0 ? STEREO : m === 2 ? MONO : PINGPONG
}

/** echospace::DIVISION_BEATS: quarter notes per DIVISION_LABELS entry. */
const DIVISION_BEATS = [
  1 / 12, 0.125, 1 / 6, 0.1875, 0.25, 1 / 3, 0.375, 0.5, 2 / 3, 0.75, 1, 4 / 3, 1.5, 2, 8 / 3, 3, 4, 6,
]
const MAX_DELAY_MS = 4000

/** LiveStage has no transport and never hands EchoSpace a tempo, so its DSP
 *  runs synced divisions at its own `DEFAULT_TEMPO_BPM`. The display follows
 *  the same figure so it draws what plays; it is not shown as a readout. */
const DSP_TEMPO_BPM = 120

/** echospace::division_ms */
function divisionMs(division: number, tempo: number): number {
  const beats = DIVISION_BEATS[clamp(Math.round(division), 0, DIVISION_BEATS.length - 1)]
  return clamp((beats * 60000) / clamp(tempo, 20, 999), 1, MAX_DELAY_MS)
}

interface EchoParams {
  mode: number
  timeL: number
  timeR: number
  feedback: number
  cross: number
  lowCut: number
  highCut: number
  freeze: boolean
  sync: boolean
  divisionL: number
  divisionR: number
}

function echoParams(editor: Editor): EchoParams {
  const v = editor.value
  return {
    mode: delayMode(v('mode')),
    timeL: v('timeMsL'),
    timeR: v('timeMsR'),
    feedback: v('feedback'),
    cross: v('crossFeedback'),
    lowCut: v('lowCutHz'),
    highCut: v('highCutHz'),
    freeze: editor.flag('freeze'),
    sync: editor.flag('sync'),
    divisionL: Math.round(v('divisionL')),
    divisionR: Math.round(v('divisionR')),
  }
}

/** Params::effective_time_ms_l / _r */
function echoTimes(p: EchoParams, tempo: number): [number, number] {
  return p.sync ? [divisionMs(p.divisionL, tempo), divisionMs(p.divisionR, tempo)] : [p.timeL, p.timeR]
}

interface Echo {
  atMs: number
  gain: number
  right: boolean
}

/** Lowest repeat the echo display draws, and how many. */
const ECHO_FLOOR_DB = -48
const MAX_ECHOES = 96

/** echospace::echo_pattern: the repeats a centred hit produces, down to the
 *  floor, by the DSP's routing and gains (wow, diffusion and drive aside). */
function echoPattern(p: EchoParams, tempo: number, sampleRate: number): Echo[] {
  const times = echoTimes(p, tempo)
  const tone = dbToLinear(cutsDb(p.lowCut, p.highCut, 1000, sampleRate))
  const fb = p.freeze ? 0.9999 : clamp(p.feedback / 100, 0, 0.98)
  const cross = p.mode !== MONO ? clamp(p.cross / 100, 0, 1) : 0
  const norm = fb / (1 + cross)
  const floor = dbToLinear(ECHO_FLOOR_DB)
  // Paths are pruned well under the floor, not at it: two that land
  // together can sum above it.
  const keep = floor * 0.01
  // Signal waiting to come out of a line: [line, written at ms, gain].
  const pending: [number, number, number][] =
    p.mode === STEREO
      ? [
          [0, 0, tone],
          [1, 0, tone],
        ]
      : [[0, 0, tone]]
  const echoes: Echo[] = []
  while (pending.length > 0) {
    // The earliest arrival (the first of equals, as Rust's min_by).
    let index = 0
    for (let i = 1; i < pending.length; i++) {
      if (pending[i][1] + times[pending[i][0]] < pending[index][1] + times[pending[index][0]]) index = i
    }
    if (echoes.length >= MAX_ECHOES) break
    const [lineIndex, written, gain] = pending[index]
    // Vec::swap_remove, so later picks see the same order the DSP crate's do.
    pending[index] = pending[pending.length - 1]
    pending.pop()
    const at = written + times[lineIndex]
    if (gain >= floor) {
      if (p.mode === MONO) {
        echoes.push({ atMs: at, gain, right: false }, { atMs: at, gain, right: true })
      } else {
        echoes.push({ atMs: at, gain, right: lineIndex === 1 })
      }
    }
    const [own, other] =
      p.mode === STEREO ? [norm, norm * cross] : p.mode === PINGPONG ? [norm * cross, norm] : [fb, 0]
    for (const [target, share] of [
      [lineIndex, own],
      [1 - lineIndex, other],
    ]) {
      const next = gain * share * tone
      if (next < keep || share === 0) continue
      // Two paths landing together are one repeat, as the line hears it.
      const merged = pending.find((q) => q[0] === target && Math.abs(q[1] - at) < 0.05)
      if (merged) merged[2] += next
      else pending.push([target, at, next])
    }
    if (pending.length > MAX_ECHOES * 4) break
  }
  return echoes
}

/** Passes the tone display draws: how the repeats darken. */
const TONE_PASSES = [1, 2, 4, 8]

// ── VerbSpace's model (verbspace/src/lib.rs) ────────────────────────────

const VERB_MODES = [
  [0, 'Room'],
  [1, 'Chamber'],
  [2, 'Hall'],
  [3, 'Plate'],
  [4, 'Ambience'],
] as const

/** ReverbMode::from_wire */
const reverbMode = (v: number) => {
  const m = Math.round(v)
  return m === 0 || m === 1 || m === 3 || m === 4 ? m : 2
}

/** ReverbMode::space: the knobs a space type's starting point sets, and
 *  each type's values for them, in the same order. The mode itself is a
 *  label the DSP never reads. */
const SPACE_IDS = [
  'predelayMs',
  'size',
  'decaySec',
  'diffusion',
  'damping',
  'bassMult',
  'bassFreqHz',
  'dampFreqHz',
  'earlyLate',
  'modDepth',
  'modRateHz',
] as const
const TYPE_SPACE: number[][] = [
  [4, 22, 0.7, 70, 45, 1.0, 250, 4500, 40, 12, 0.6], // Room
  [10, 40, 1.4, 85, 35, 1.1, 250, 5000, 55, 20, 0.7], // Chamber
  [20, 60, 2.4, 80, 40, 1.2, 250, 4000, 65, 25, 0.7], // Hall
  [6, 35, 1.8, 95, 15, 0.8, 400, 8000, 100, 30, 1.0], // Plate: no reflections
  [0, 12, 0.5, 60, 50, 0.9, 250, 5000, 25, 10, 0.5], // Ambience
]

/** ReverbMode::starting_point, as the wire values it sends: the label and
 *  the type's space. Cuts, width, mix, output and the switches stay. */
function startingPoint(mode: number): Record<string, number> {
  const values: Record<string, number> = { mode }
  SPACE_IDS.forEach((id, i) => {
    values[id] = TYPE_SPACE[mode][i]
  })
  return values
}

const LINE_COUNT = 16
const MAX_PREDELAY_MS = 500
const FREEZE_GAIN = 0.99995
const MAX_RT60_SEC = 60
const MIN_HIGH_RATIO = 0.1
const MIN_LONGEST_LINE_MS = 16
const MAX_LONGEST_LINE_MS = 180
const SHORTEST_LINE_SHARE = 0.32
const MIN_EARLY_SPAN_MS = 5
const MAX_EARLY_SPAN_MS = 140
const LINE_SPREAD_JITTER = [
  0.0, 0.21, -0.17, 0.31, -0.08, 0.27, -0.29, 0.12, -0.22, 0.33, -0.11, 0.19, -0.31, 0.07, -0.24, 0.0,
]
const LINE_ORDER = [0, 9, 3, 12, 6, 15, 1, 10, 4, 13, 7, 2, 11, 5, 14, 8]
const TANK_ALLPASS_MS = [
  0.61, 0.73, 0.89, 0.97, 1.13, 1.27, 1.39, 1.51, 1.67, 1.79, 1.93, 2.11, 2.29, 2.41, 2.63, 2.87,
]
const EARLY_AT_L = [0.043, 0.087, 0.131, 0.179, 0.233, 0.297, 0.367, 0.443, 0.531, 0.629, 0.743, 0.887]
const EARLY_AT_R = [0.051, 0.097, 0.149, 0.199, 0.257, 0.319, 0.389, 0.471, 0.557, 0.661, 0.781, 0.937]
const EARLY_SIGN = [1, -1, 1, 1, -1, 1, -1, -1, 1, -1, 1, -1]

/** `size` 0–100 % onto 0–1, and the exponential travel every Size curve
 *  takes (space, sweep). */
const space = (size: number) => clamp(size / 100, 0, 1)
const sweep = (min: number, max: number, at: number) => min * Math.pow(max / min, at)

/** high_ratio: the top band's share of the middle band's decay. */
const highRatio = (damping: number) => Math.pow(MIN_HIGH_RATIO, clamp(damping / 100, 0, 1))

/** early_late_gains: the (early, late) output gains, equal power. */
function earlyLateGains(earlyLate: number): [number, number] {
  const b = clamp(earlyLate / 100, 0, 1)
  if (b >= 1) return [0, 1]
  if (b <= 0) return [1, 0]
  return [Math.cos((b * Math.PI) / 2), Math.sin((b * Math.PI) / 2)]
}

/** Bilinear one-pole low-pass (OnePole::a_for, from_a). */
interface OnePole {
  a: number
  b: number
}

function onePole(cornerHz: number, sampleRate: number): OnePole {
  const k = Math.tan((Math.PI * clamp(cornerHz, 1, sampleRate * 0.45)) / sampleRate)
  const a = (1 - k) / (1 + k)
  return { a, b: (1 - a) * 0.5 }
}

/** The one-pole's complex response at `hz`, as [re, im]. */
function onePoleAt(p: OnePole, hz: number, sampleRate: number): [number, number] {
  const w = (2 * Math.PI * hz) / sampleRate
  const [numRe, numIm] = [p.b * (1 + Math.cos(w)), -p.b * Math.sin(w)]
  const [denRe, denIm] = [1 - p.a * Math.cos(w), p.a * Math.sin(w)]
  const den = denRe * denRe + denIm * denIm
  return [(numRe * denRe + numIm * denIm) / den, (numIm * denRe - numRe * denIm) / den]
}

/** One line's per-pass loss: gain × low shelf × high shelf. */
interface Absorption {
  gain: number
  low: number
  high: number
}

interface EarlyReflection {
  atMs: number
  gain: number
  right: boolean
}

/** verbspace::DecayProfile, with what `rt_at` reads. */
interface DecayProfile {
  predelayMs: number
  early: EarlyReflection[]
  firstLateMs: number
  lateDb: number
  rtLow: number
  rtMid: number
  rtHigh: number
  frozen: boolean
  rtAt(hz: number): number
}

function earlyGains(at: number[]): number[] {
  const raw = at.map((a, i) => (1 - 0.75 * a) * EARLY_SIGN[i])
  const norm = Math.sqrt(raw.reduce((sum, g) => sum + g * g, 0))
  return raw.map((g) => g / norm)
}

/** verbspace::decay_profile: Tuning::resolve's tank and the decay a typical
 *  line implies. */
function decayProfile(editor: Editor, sampleRate: number): DecayProfile {
  const v = editor.value
  const sr = Math.max(1, sampleRate)
  const ms = (m: number) => m * 0.001 * sr
  const at = space(v('size'))
  const longest = sweep(MIN_LONGEST_LINE_MS, MAX_LONGEST_LINE_MS, at)
  const shortest = longest * SHORTEST_LINE_SHARE
  const lines = new Array<number>(LINE_COUNT).fill(0)
  for (let rank = 0; rank < LINE_COUNT; rank++) {
    const spread = clamp((rank + LINE_SPREAD_JITTER[rank]) / (LINE_COUNT - 1), 0, 1)
    lines[LINE_ORDER[rank]] = Math.max(4, ms(sweep(shortest, longest, spread)))
  }
  const loops = lines.map((l, i) => l + Math.max(1, Math.round(TANK_ALLPASS_MS[i] * 0.001 * sr)))
  const decay = v('decaySec')
  const rtMid = clamp(decay, 0.05, MAX_RT60_SEC)
  const rtLow = clamp(decay * v('bassMult'), 0.05, MAX_RT60_SEC)
  const rtHigh = clamp(rtMid * highRatio(v('damping')), 0.02, MAX_RT60_SEC)
  const frozen = editor.flag('freeze')
  const predelay = ms(clamp(v('predelayMs'), 0, MAX_PREDELAY_MS))
  const earlySpan = ms(sweep(MIN_EARLY_SPAN_MS, MAX_EARLY_SPAN_MS, at))
  const [earlyGain, lateGain] = earlyLateGains(v('earlyLate'))
  const toMs = 1000 / sr
  const gainsL = earlyGains(EARLY_AT_L)
  const gainsR = earlyGains(EARLY_AT_R)
  const early: EarlyReflection[] = [
    ...EARLY_AT_L.map((a, i) => ({ atMs: (predelay + a * earlySpan) * toMs, gain: gainsL[i] * earlyGain, right: false })),
    ...EARLY_AT_R.map((a, i) => ({ atMs: (predelay + a * earlySpan) * toMs, gain: gainsR[i] * earlyGain, right: true })),
  ]
  // The line whose loop is the median: "a typical line".
  const order = loops.map((_, i) => i).sort((a, b) => loops[a] - loops[b])
  const typical = order[LINE_COUNT / 2]
  const loop = loops[typical]
  const perPass = (rt: number) => Math.pow(10, (-3 * loop) / (sr * rt))
  const absorption: Absorption = frozen
    ? { gain: FREEZE_GAIN, low: 1, high: 1 }
    : { gain: perPass(rtMid), low: perPass(rtLow) / perPass(rtMid), high: perPass(rtHigh) / perPass(rtMid) }
  const lowSplit = onePole(v('bassFreqHz'), sr)
  const highSplit = onePole(v('dampFreqHz'), sr)
  const rt = (sec: number) => (frozen ? Infinity : sec)
  return {
    predelayMs: predelay * toMs,
    early,
    firstLateMs: Math.min(...lines) * toMs,
    lateDb: 20 * log10(Math.max(1e-6, lateGain)),
    rtLow: rt(rtLow),
    rtMid: rt(rtMid),
    rtHigh: rt(rtHigh),
    frozen,
    rtAt(hz) {
      if (frozen) return Infinity
      // Low shelf 1 + (low − 1)·LP, high shelf high + (1 − high)·LP.
      const [lr, li] = onePoleAt(lowSplit, hz, sr)
      const [hr, hi] = onePoleAt(highSplit, hz, sr)
      const lowShelf = Math.hypot(1 + (absorption.low - 1) * lr, (absorption.low - 1) * li)
      const highShelf = Math.hypot(absorption.high + (1 - absorption.high) * hr, (1 - absorption.high) * hi)
      const magnitude = Math.max(1e-12, absorption.gain * lowShelf * highShelf)
      return (-3 * (loop / sr)) / log10(magnitude)
    },
  }
}

// ── Axes and readouts (fx_panel) ────────────────────────────────────────

/** 1, 2 or 5 times a power of ten, giving about `ticks` steps across `span`. */
function niceStep(span: number, ticks: number): number {
  const raw = span / Math.max(1, ticks)
  const magnitude = Math.pow(10, Math.floor(log10(raw)))
  const scaled = raw / magnitude
  return (scaled < 1.5 ? 1 : scaled < 3.5 ? 2 : scaled < 7.5 ? 5 : 10) * magnitude
}

/** `value` rounded up to the next tidy figure on a 1-2-5 ladder. */
function niceCeiling(value: number): number {
  const magnitude = Math.pow(10, Math.floor(log10(Math.max(1e-3, value))))
  return [1, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10].map((m) => m * magnitude).find((v) => v >= value) ?? value
}

function msText(value: number): string {
  if (value < 1000) return `${value.toFixed(0)} ms`
  const s = value / 1000
  if (Math.abs(s - Math.round(s)) < 1e-3) return `${s.toFixed(0)} s`
  return s >= 10 ? `${s.toFixed(1)} s` : `${s.toFixed(2)} s`
}

function secondsText(value: number): string {
  if (!Number.isFinite(value)) return '∞'
  return value < 10 ? `${value.toFixed(2)} s` : `${value.toFixed(1)} s`
}

type Axis = [number, string][]

/** The frequencies a narrow display labels. */
const FREQ_AXIS: Axis = (
  [
    [100, '100'],
    [300, '300'],
    [1000, '1k'],
    [3000, '3k'],
    [10000, '10k'],
  ] as const
).map(([hz, text]): [number, string] => [freqFraction(hz), text])

function timeAxis(spanMs: number): Axis {
  const step = niceStep(spanMs, 5)
  const labels: Axis = []
  for (let i = 1; i * step < spanMs * 0.999; i++) labels.push([(i * step) / spanMs, msText(i * step)])
  return labels
}

/** eq_graph::FREQ_GRID */
const FREQ_GRID = [
  20, 30, 40, 50, 60, 70, 80, 90, 100, 200, 300, 400, 500, 600, 700, 800, 900, 1000, 2000, 3000, 4000, 5000,
  6000, 7000, 8000, 9000, 10000, 20000,
]

// ── Painting ────────────────────────────────────────────────────────────

/** Every colour the displays need. Native draws the low band and the right
 *  side in track colours; the page has none, and its accent is the teal the
 *  native audio-track colour is, so the low band takes the page's blue to
 *  stay apart from the mid line. */
interface Palette {
  grid: string
  gridMajor: string
  axis: string
  signal: string
  low: string
  high: string
  left: string
  right: string
  early: string
  shade: string
  dim: number
}

let tokens: Record<'success' | 'warning' | 'blue' | 'window', string> | null = null

function palette(bypassed: boolean): Palette {
  const c = colors()
  if (!tokens) {
    const s = getComputedStyle(document.documentElement)
    const v = (name: string) => s.getPropertyValue(name).trim()
    tokens = {
      success: v('--success'),
      warning: v('--warning'),
      blue: v('--state-mute'),
      window: v('--surface-window'),
    }
  }
  return {
    grid: alpha(c.text, 0.045),
    gridMajor: alpha(c.text, 0.09),
    axis: alpha(c.text, 0.18),
    signal: c.accent,
    low: tokens.blue,
    high: tokens.warning,
    left: c.accent,
    right: tokens.success,
    early: c.textSecondary,
    shade: tokens.window,
    dim: bypassed ? 0.35 : 1,
  }
}

const DECAY_FLOOR_DB = -60
const TONE_FLOOR_DB = -36

function paintFreqGrid(ctx: Ctx, w: number, h: number, p: Palette) {
  for (const hz of FREQ_GRID) {
    const major = hz === 100 || hz === 1000 || hz === 10000
    rect(ctx, freqFraction(hz) * w, 0, 1, h, major ? p.gridMajor : p.grid)
  }
}

function paintTimeGrid(ctx: Ctx, w: number, h: number, spanMs: number, p: Palette) {
  const step = niceStep(spanMs, 5)
  for (let t = step; t < spanMs; t += step) rect(ctx, (t / spanMs) * w, 0, 1, h, p.gridMajor)
}

/** The reverb's level over time: the pre-delay, the early reflections, and
 *  each band's decay down to −60 dB. */
function paintDecay(ctx: Ctx, w: number, h: number, profile: DecayProfile, spanMs: number, p: Palette) {
  paintTimeGrid(ctx, w, h, spanMs, p)
  for (const db of [-12, -24, -36, -48]) rect(ctx, 0, (db / DECAY_FLOOR_DB) * h, w, 1, p.grid)
  const xAt = (ms: number) => clamp(ms / spanMs, 0, 1) * w
  const yAt = (db: number) => clamp(db / DECAY_FLOOR_DB, 0, 1) * h

  // The pre-delay: nothing yet.
  if (profile.predelayMs > 0) rect(ctx, 0, 0, xAt(profile.predelayMs), h, alpha(p.shade, 0.55))

  // The tail of each band, from the first late arrival, starting at the
  // level the Early/Late balance gives it and falling 60 dB in its RT60.
  const onset = profile.predelayMs + profile.firstLateMs
  const startDb = Math.max(DECAY_FLOOR_DB, profile.lateDb - 3)
  const lineOf = (rt: number): [number, number][] => {
    if (!Number.isFinite(rt)) {
      return [
        [xAt(onset), yAt(startDb)],
        [w, yAt(startDb)],
      ]
    }
    const end = onset + rt * 1000
    if (end <= spanMs) {
      return [
        [xAt(onset), yAt(startDb)],
        [xAt(end), yAt(startDb - 60)],
      ]
    }
    const db = startDb - (60 * (spanMs - onset)) / (end - onset)
    return [
      [xAt(onset), yAt(startDb)],
      [w, yAt(db)],
    ]
  }
  const mid = lineOf(profile.rtMid)
  area(ctx, mid, h, alpha(p.signal, 0.12 * p.dim))
  line(ctx, lineOf(profile.rtLow), 1.2, alpha(p.low, 0.85 * p.dim))
  line(ctx, lineOf(profile.rtHigh), 1.2, alpha(p.high, 0.85 * p.dim))
  line(ctx, mid, 2, alpha(p.signal, p.dim))

  // Early reflections, as ticks from the floor: left a touch to the left of
  // right, so coincident pairs both show.
  for (const reflection of profile.early) {
    if (reflection.gain === 0) continue
    const db = 20 * log10(Math.max(1e-6, Math.abs(reflection.gain)))
    const x = xAt(reflection.atMs) + (reflection.right ? 1 : -1)
    const top = yAt(Math.max(DECAY_FLOOR_DB, db))
    rect(ctx, x, top, 1.5, h - top, alpha(reflection.right ? p.right : p.left, 0.7 * p.dim))
  }
  rect(ctx, 0, yAt(0), w, 1, p.axis)
}

/** RT60 across frequency, with the wet cuts shading what never leaves. */
function paintRt(ctx: Ctx, w: number, h: number, rt: number[], cuts: number[], topSec: number, p: Palette) {
  paintFreqGrid(ctx, w, h, p)
  const column = w / (CURVE_POINTS - 1)
  cuts.forEach((db, i) => {
    const amount = clamp(-db / 24, 0, 1)
    if (amount > 0.02) rect(ctx, i * column - column * 0.5, 0, column + 0.5, h, alpha(p.shade, 0.75 * amount))
  })
  const points = rt.map((sec, i): [number, number] => {
    const level = Number.isFinite(sec) ? sec / topSec : 1
    return [freqFraction(curveHz(i)) * w, h - clamp(level, 0, 1) * (h - 4)]
  })
  area(ctx, points, h, alpha(p.signal, 0.12 * p.dim))
  line(ctx, points, 2, alpha(p.signal, p.dim))
}

/** The repeats: left above the centre line, right below, each as tall as it
 *  is loud; the beat grid behind them while synced. */
function paintRepeats(
  ctx: Ctx,
  w: number,
  h: number,
  echoes: Echo[],
  spanMs: number,
  beatMs: number | null,
  mono: boolean,
  p: Palette,
) {
  if (beatMs !== null && spanMs / beatMs < 96) {
    for (let i = 1; i * beatMs < spanMs; i++) {
      rect(ctx, ((i * beatMs) / spanMs) * w, 0, 1, h, i % 4 === 0 ? p.gridMajor : p.grid)
    }
  } else {
    paintTimeGrid(ctx, w, h, spanMs, p)
  }
  const centre = h * 0.5
  rect(ctx, 0, centre, w, 1, p.axis)
  // The dry hit.
  rect(ctx, 0, 6, 2, h - 12, alpha(p.early, 0.5))
  const lane = h * 0.5 - 6
  for (const echo of echoes) {
    if (echo.atMs > spanMs) continue
    const db = 20 * log10(Math.max(1e-6, echo.gain))
    const level = clamp((db - ECHO_FLOOR_DB) / -ECHO_FLOOR_DB, 0, 1)
    const length = Math.max(1, level * lane)
    const x = (echo.atMs / spanMs) * w - 1
    const color = alpha(echo.right && !mono ? p.right : p.left, (0.35 + 0.65 * level) * p.dim)
    rect(ctx, x, echo.right ? centre + 1 : centre - length, 2.5, length, color)
  }
}

/** What the tone stage leaves after each of TONE_PASSES. */
function paintTone(ctx: Ctx, w: number, h: number, tones: number[][], p: Palette) {
  paintFreqGrid(ctx, w, h, p)
  for (const db of [-12, -24]) rect(ctx, 0, (db / TONE_FLOOR_DB) * h, w, 1, p.grid)
  for (let pass = tones.length - 1; pass >= 0; pass--) {
    const points = tones[pass].map((db, i): [number, number] => [
      freqFraction(curveHz(i)) * w,
      3 + clamp(db / TONE_FLOOR_DB, 0, 1) * (h - 6),
    ])
    if (pass === 0) area(ctx, points, h, alpha(p.signal, 0.1 * p.dim))
    line(ctx, points, pass === 0 ? 2 : 1.2, alpha(p.signal, [1, 0.6, 0.38, 0.22][Math.min(3, pass)] * p.dim))
  }
}

// ── Displays ────────────────────────────────────────────────────────────

/** A framed display: a caption and a legend over its top edge, a notice
 *  along its bottom, and its axis labels under it. */
function Display(props: {
  title: string
  legend?: string
  notice?: string | null
  grow: number
  axis: Axis
  draw: (ctx: Ctx, w: number, h: number) => void
}) {
  return (
    <div className="fx-display" style={{ flexGrow: props.grow }}>
      <LiveCanvas className="fx-plot" draw={props.draw}>
        <div className="fx-tags">
          <span className="fx-tag">{props.title}</span>
          {props.legend && <span className="fx-tag">{props.legend}</span>}
        </div>
        {props.notice && (
          <div className="fx-notice">
            <span>{props.notice}</span>
          </div>
        )}
      </LiveCanvas>
      <div className="fx-axis" aria-hidden>
        {props.axis.map(([fraction, text]) => (
          <span key={`${fraction}-${text}`} style={{ left: `${fraction * 100}%` }}>
            {text}
          </span>
        ))}
      </div>
    </div>
  )
}

const bypassNotice = (editor: Editor, title: string) =>
  editor.bypassed ? `Bypassed — ${title} passes audio through unchanged` : null

function EchoDisplays(props: { editor: Editor }) {
  const { editor } = props
  const p = echoParams(editor)
  const sr = editor.sampleRate
  const echoes = echoPattern(p, DSP_TEMPO_BPM, sr)
  const once = Array.from({ length: CURVE_POINTS }, (_, i) => cutsDb(p.lowCut, p.highCut, curveHz(i), sr))
  const tones = TONE_PASSES.map((passes) => once.map((db) => db * passes))
  const times = echoTimes(p, DSP_TEMPO_BPM)
  // The repeat display's span: past the last repeat drawn.
  const last = echoes.reduce((m, e) => Math.max(m, e.atMs), 0)
  const span = niceCeiling(clamp(Math.max(last, Math.max(times[0], times[1]) * 2) * 1.06, 100, 30000))
  const beatMs = p.sync ? 60000 / DSP_TEMPO_BPM : null
  const mono = p.mode === MONO
  let legend = mono ? msText(times[0]) : `L ${msText(times[0])} · R ${msText(times[1])}`
  if (p.sync) {
    const label = (d: number) => DIVISION_LABELS[Math.min(17, d)]
    legend = mono
      ? `${label(p.divisionL)} — ${legend}`
      : `${label(p.divisionL)} · ${label(p.divisionR)} — ${legend}`
  }
  const pal = palette(editor.bypassed)
  return (
    <div className="fx-displays">
      <Display
        title="REPEATS"
        legend={legend}
        notice={bypassNotice(editor, 'EchoSpace')}
        grow={3}
        axis={timeAxis(span)}
        draw={(ctx, w, h) => paintRepeats(ctx, w, h, echoes, span, beatMs, mono, pal)}
      />
      <Display
        title="TONE PER PASS"
        legend="1 · 2 · 4 · 8 passes"
        grow={2}
        axis={FREQ_AXIS}
        draw={(ctx, w, h) => paintTone(ctx, w, h, tones, pal)}
      />
    </div>
  )
}

function VerbDisplays(props: { editor: Editor }) {
  const { editor } = props
  const sr = editor.sampleRate
  const profile = decayProfile(editor, sr)
  const rt = Array.from({ length: CURVE_POINTS }, (_, i) => profile.rtAt(curveHz(i)))
  const lowCut = editor.value('lowCutHz')
  const highCut = editor.value('highCutHz')
  const cuts = Array.from({ length: CURVE_POINTS }, (_, i) => cutsDb(lowCut, highCut, curveHz(i), sr))
  // The decay display's span: past the slowest band's RT60, tidied.
  const tail = profile.frozen ? 4 : Math.max(profile.rtLow, profile.rtMid, profile.rtHigh)
  const span = niceCeiling(clamp(profile.predelayMs + tail * 1000 * 1.08, 200, 45000))
  const rtTop = niceCeiling(rt.filter(Number.isFinite).reduce((m, s) => Math.max(m, s), 0.5) * 1.15)
  const legend = profile.frozen
    ? 'Frozen — the tail holds'
    : `Low ${secondsText(profile.rtLow)} · Mid ${secondsText(profile.rtMid)} · High ${secondsText(profile.rtHigh)}`
  const pal = palette(editor.bypassed)
  return (
    <div className="fx-displays">
      <Display
        title="DECAY"
        legend={legend}
        notice={bypassNotice(editor, 'VerbSpace')}
        grow={3}
        axis={timeAxis(span)}
        draw={(ctx, w, h) => paintDecay(ctx, w, h, profile, span, pal)}
      />
      <Display
        title="DECAY BY FREQUENCY"
        legend={`RT60, 0–${secondsText(rtTop)}`}
        grow={2}
        axis={FREQ_AXIS}
        draw={(ctx, w, h) => paintRt(ctx, w, h, rt, cuts, rtTop, pal)}
      />
    </div>
  )
}

// ── The editors ─────────────────────────────────────────────────────────

/** The listening state a preset or A/B swap leaves alone (preset_applied). */
const KEEP = ['freeze']
/** VerbSpace's, with its Wet Only routing. */
const VERB_KEEP = ['freeze', 'wetOnly']

function VerbSpaceEditor(props: { editor: Editor }) {
  const { editor } = props
  const wetOnly = editor.flag('wetOnly')
  const knob = (id: string, why?: string) => <FxKnob key={id} editor={editor} kind="verb" id={id} why={why} />
  const knobs = (ids: string[]) => ids.map((id) => knob(id))
  return (
    <EditorShell editor={editor} title="VerbSpace" subtitle="Algorithmic reverb" keep={VERB_KEEP}>
      <div className="fx-toolbar">
        <span className="fx-caption">START FROM</span>
        {/* A type is a starting point: it moves the knobs to its space, and
            the knobs alone shape the sound. */}
        <Choice
          value={reverbMode(editor.value('mode'))}
          options={VERB_MODES}
          onChange={(mode) => editor.setMany(startingPoint(mode))}
        />
        <span className="spacer" />
        <Check label="Wet Only" checked={wetOnly} onChange={(on) => editor.set('wetOnly', on ? 1 : 0)} />
        <Check label="Freeze" checked={editor.flag('freeze')} onChange={(on) => editor.set('freeze', on ? 1 : 0)} />
      </div>
      <VerbDisplays editor={editor} />
      <Row>
        <Section title="Space" count={5}>
          {knobs(['predelayMs', 'size', 'decaySec', 'diffusion', 'earlyLate'])}
        </Section>
        <Section title="Decay colour" count={4}>
          {knobs(['bassMult', 'bassFreqHz', 'damping', 'dampFreqHz'])}
        </Section>
        <Section title="Motion" count={2}>
          {knobs(['modDepth', 'modRateHz'])}
        </Section>
        <Section title="Output" count={5}>
          {knob('lowCutHz')}
          {knob('highCutHz')}
          {knob('width')}
          {knob('mix', wetOnly ? 'wet only' : undefined)}
          {knob('outputDb')}
        </Section>
      </Row>
    </EditorShell>
  )
}

/** Each side's twin: while Link is on the DSP mirrors an edit onto it. */
const TWIN: Record<string, string> = {
  timeMsL: 'timeMsR',
  timeMsR: 'timeMsL',
  divisionL: 'divisionR',
  divisionR: 'divisionL',
}
const SIDES = Object.keys(TWIN)

function EchoSpaceEditor(props: { editor: Editor }) {
  const { editor } = props
  const linked = editor.flag('link')
  const mode = delayMode(editor.value('mode'))
  const mono = mode === MONO
  const sync = editor.flag('sync')

  // The session keeps each wire value as sent, so a linked edit sends both
  // sides, as the DSP will hold them (ipc::apply_wire_param).
  const sides: Editor = {
    ...editor,
    set: (id, value) => {
      editor.set(id, value)
      const twin = TWIN[id]
      if (twin && editor.flag('link')) editor.set(twin, value)
    },
  }

  // Native sends `link` first, so a preset or A/B swap that turns it off
  // lands its two sides unmirrored. The shell sends in wire order — the sides
  // ahead of `link` — so when Link goes off, send the sides again after it.
  const wasLinked = useRef(linked)
  useEffect(() => {
    if (wasLinked.current && !linked) {
      const values = SIDES.map((id): [number, number] => [editor.spec.ids.indexOf(id), editor.value(id)]).filter(
        ([i]) => i >= 0,
      )
      act({ cmd: 'set_insert_params', insert: editor.insert, values })
    }
    wasLinked.current = linked
  })

  const toggleLink = () => {
    if (linked) editor.set('link', 0)
    // Turning Link on pulls the right side onto the left (ipc::apply_link).
    else editor.setMany({ link: 1, timeMsR: editor.value('timeMsL'), divisionR: editor.value('divisionL') })
  }

  const why = mono ? 'mono' : undefined
  const [left, right] = sync ? ['divisionL', 'divisionR'] : ['timeMsL', 'timeMsR']
  const knob = (id: string, reason?: string) => (
    <FxKnob key={id} editor={sides} kind="echo" id={id} why={reason} />
  )

  return (
    <EditorShell editor={editor} title="EchoSpace" subtitle="Stereo delay" keep={KEEP}>
      <div className="fx-toolbar">
        <span className="fx-caption">MODE</span>
        <ParamChoice editor={editor} id="mode" options={ECHO_MODES} />
        <Check
          label="Sync"
          title="Note divisions. LiveStage has no transport, so they run at EchoSpace's own default tempo."
          checked={sync}
          onChange={(on) => editor.set('sync', on ? 1 : 0)}
        />
        <label className={`pe-check${mono ? ' fx-off' : ''}`}>
          <input type="checkbox" checked={linked} disabled={mono} onChange={toggleLink} />
          <span>Link L/R</span>
        </label>
        <span className="spacer" />
        <Check label="Freeze" checked={editor.flag('freeze')} onChange={(on) => editor.set('freeze', on ? 1 : 0)} />
      </div>
      <EchoDisplays editor={editor} />
      <Row>
        <Section title="Time" count={2}>
          {knob(left)}
          {knob(right, why)}
        </Section>
        <Section title="Feedback" count={3}>
          {knob('feedback')}
          {knob('crossFeedback', why)}
          {knob('diffusion')}
        </Section>
        <Section title="Tone" count={3}>
          {knob('lowCutHz')}
          {knob('highCutHz')}
          {knob('saturation')}
        </Section>
        <Section title="Motion" count={2}>
          {knob('modDepth')}
          {knob('modRateHz')}
        </Section>
        <Section title="Output" count={4}>
          {knob('duck')}
          {knob('width', why)}
          {knob('mix')}
          {knob('outputDb')}
        </Section>
      </Row>
    </EditorShell>
  )
}

export const editors: Record<string, EditorComponent> = {
  echospace: EchoSpaceEditor,
  verbspace: VerbSpaceEditor,
}
