// MixStation: a port of the native editor (components/mix_station_model.rs,
// mix_station_panel.rs). A channel strip whose six modules run in the order
// the user stacks them, in three columns:
//
// * the signal path: the rack, first module first, each row with its rack
//   position's live levels and its switch; drag a row's grip to move it, and
//   add a module from the list of those not yet in the rack;
// * the selected module: its display, computed from the DSP's own models
//   (mixstation lib.rs / dsp.rs), with handles to drag, and its knobs and trim;
// * the strip's input and output trims around its meters.
//
// The rack is plain wire params, `slot1Module`…`slot6Module`, each holding a
// module code 1–6 (0 = empty). The DSP keeps a module in one slot at most:
// writing a module to a slot clears it from every other. So every rack move
// sends all six slots, first to last, and lands exactly on the order written.

import { useEffect, useRef, useState } from 'react'
import type { KeyboardEvent, PointerEvent } from 'react'
import type { Editor, EditorComponent } from './kit.tsx'
import { DisplayTag, EditorShell, KitKnob, KNOB, LiveCanvas } from './kit.tsx'
import type { KnobSpec, Taper, Unit } from './knobspec.ts'
import type { Ctx } from './paint.ts'
import {
  alpha,
  area,
  colors,
  dashed,
  DENSE_CAPTION,
  dot,
  freqAtFraction,
  freqFraction,
  label,
  line,
  paintMeters,
  paintSpectrum,
  paintStageBars,
  rect,
  transferPlot,
} from './paint.ts'
import './mixstation.css'

// ── The model (mix_station_model.rs) ────────────────────────────────────

const SLOTS = 6
const SLOT_IDS = ['slot1Module', 'slot2Module', 'slot3Module', 'slot4Module', 'slot5Module', 'slot6Module']

interface Module {
  /** Its code on the wire, 1 to 6. */
  code: number
  name: string
  hint: string
  /** The param switching it in and out of the path. */
  enabled: string
  knobs: string[]
  /** Its own output trim, which travels with it through the rack. */
  trim: string
}

const MODULES: Module[] = [
  { code: 1, name: 'Filters', hint: '24 dB/oct high and low cut', enabled: 'filtersEnabled', knobs: ['hpfHz', 'lpfHz'], trim: 'filtersTrimDb' },
  {
    code: 2,
    name: 'EQ',
    hint: 'Four-band with proportional-Q mids',
    enabled: 'eqEnabled',
    knobs: ['lowGainDb', 'lowMidFreqHz', 'lowMidGainDb', 'highMidFreqHz', 'highMidGainDb', 'highGainDb'],
    trim: 'eqTrimDb',
  },
  {
    code: 3,
    name: 'Compressor',
    hint: 'Stereo-linked, program-dependent release',
    enabled: 'compEnabled',
    knobs: ['compThresholdDb', 'compRatio', 'compAttackMs', 'compReleaseMs', 'compMakeupDb'],
    trim: 'compTrimDb',
  },
  { code: 4, name: 'Drive', hint: 'Anti-aliased asymmetric saturation', enabled: 'satEnabled', knobs: ['satDrivePct', 'satCharacterPct'], trim: 'satTrimDb' },
  { code: 5, name: 'Width', hint: 'Mid/side stereo image', enabled: 'widthEnabled', knobs: ['widthPct'], trim: 'widthTrimDb' },
  { code: 6, name: 'Limiter', hint: 'Zero-latency brickwall ceiling', enabled: 'limiterEnabled', knobs: ['limiterCeilingDb', 'limiterReleaseMs'], trim: 'limiterTrimDb' },
]

const moduleOf = (code: number) => MODULES.find((m) => m.code === code)

/** One loaded rack position: the module, and the wire slot holding it (the
 *  DSP meters by slot). */
interface Stage {
  code: number
  slot: number
}

/** The modules in the rack, first processed first. */
function rackOf(editor: Editor): Stage[] {
  const seen = new Set<number>()
  const stages: Stage[] = []
  SLOT_IDS.forEach((id, slot) => {
    const code = Math.round(editor.value(id))
    if (moduleOf(code) && !seen.has(code)) {
      seen.add(code)
      stages.push({ code, slot })
    }
  })
  return stages
}

/** The slot values holding `order`, the rest empty, keyed first slot to last
 *  so `setMany` sends them in that order. */
function slotsFor(order: number[]): Record<string, number> {
  const values: Record<string, number> = {}
  SLOT_IDS.forEach((id, slot) => {
    values[id] = order[slot] ?? 0
  })
  return values
}

const KNOBS: Record<string, [string, Taper, Unit]> = {
  inputTrimDb: ['Input', 'linear', 'db'],
  outputTrimDb: ['Output', 'linear', 'db'],
  hpfHz: ['Low Cut', 'log', 'hz'],
  lpfHz: ['High Cut', 'log', 'hz'],
  lowGainDb: ['Low', 'linear', 'db'],
  lowMidFreqHz: ['LM Freq', 'log', 'hz'],
  lowMidGainDb: ['LM Gain', 'linear', 'db'],
  highMidFreqHz: ['HM Freq', 'log', 'hz'],
  highMidGainDb: ['HM Gain', 'linear', 'db'],
  highGainDb: ['High', 'linear', 'db'],
  compThresholdDb: ['Threshold', 'linear', 'db'],
  compRatio: ['Ratio', 'log', 'ratio'],
  compAttackMs: ['Attack', 'log', 'ms'],
  compReleaseMs: ['Release', 'log', 'ms'],
  compMakeupDb: ['Makeup', 'linear', 'db'],
  satDrivePct: ['Drive', 'linear', 'percent'],
  satCharacterPct: ['Character', 'linear', 'percent'],
  widthPct: ['Width', 'linear', 'percent'],
  limiterCeilingDb: ['Ceiling', 'linear', 'db'],
  limiterReleaseMs: ['Release', 'log', 'ms'],
}

/** mix_station_model::knob: the range from the descriptor, the rest here. */
function knobOf(editor: Editor, id: string): KnobSpec | null {
  const range = editor.effect.params.find((p) => p.id === id)
  if (!range) return null
  const [label, taper, unit] = KNOBS[id] ?? (id.endsWith('TrimDb') ? ['Trim', 'linear', 'db'] : [id, 'linear', 'plain'])
  const knob: KnobSpec = { id, label, min: range.min, max: range.max, taper, unit }
  if (['lowGainDb', 'lowMidGainDb', 'highMidGainDb', 'highGainDb', 'compMakeupDb'].includes(id) || id.endsWith('TrimDb')) {
    knob.bipolar = true
    knob.centre = 0
  } else if (id === 'widthPct') {
    knob.bipolar = true
    knob.centre = 100
  }
  return knob
}

/** A knob on wire id `id`, as `knobOf` describes it. */
function ParamKnob(props: { editor: Editor; id: string; size: number }) {
  const spec = knobOf(props.editor, props.id)
  return spec && <KitKnob editor={props.editor} spec={spec} size={props.size} />
}

// ── The DSP's own curves (mixstation lib.rs, dsp.rs) ────────────────────

const LOW_SHELF_HZ = 100
const HIGH_SHELF_HZ = 10_000
const HPF_OPEN_HZ = 20
const LPF_OPEN_HZ = 20_000
const COMP_KNEE_DB = 6
const LIMITER_KNEE_DB = 1.5
const SATURATION_DRIVE_SCALE = 0.06
const SATURATION_BIAS = 0.25
const BUTTERWORTH_4_Q = [0.5411961, 1.3065629]

type Coeffs = [number, number, number, number, number]

function omega(sr: number, hz: number): number {
  const rate = Math.max(1, sr)
  return (2 * Math.PI * Math.min(rate * 0.49, Math.max(10, hz))) / rate
}

function normalized(b0: number, b1: number, b2: number, a0: number, a1: number, a2: number): Coeffs {
  return [b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0]
}

function highPass(sr: number, hz: number, q: number): Coeffs {
  const w = omega(sr, hz)
  const cos = Math.cos(w)
  const a = Math.sin(w) / (2 * Math.max(0.1, q))
  return normalized((1 + cos) / 2, -(1 + cos), (1 + cos) / 2, 1 + a, -2 * cos, 1 - a)
}

function lowPass(sr: number, hz: number, q: number): Coeffs {
  const w = omega(sr, hz)
  const cos = Math.cos(w)
  const a = Math.sin(w) / (2 * Math.max(0.1, q))
  return normalized((1 - cos) / 2, 1 - cos, (1 - cos) / 2, 1 + a, -2 * cos, 1 - a)
}

function peak(sr: number, hz: number, gainDb: number, q: number): Coeffs {
  const A = Math.pow(10, gainDb / 40)
  const w = omega(sr, hz)
  const cos = Math.cos(w)
  const a = Math.sin(w) / (2 * Math.max(0.1, q))
  return normalized(1 + a * A, -2 * cos, 1 - a * A, 1 + a / A, -2 * cos, 1 - a / A)
}

function shelf(sr: number, hz: number, gainDb: number, high: boolean): Coeffs {
  const A = Math.pow(10, gainDb / 40)
  const w = omega(sr, hz)
  const cos = Math.cos(w)
  const two = 2 * Math.sqrt(A) * Math.sin(w) * Math.SQRT1_2
  if (!high) {
    return normalized(
      A * (A + 1 - (A - 1) * cos + two),
      2 * A * (A - 1 - (A + 1) * cos),
      A * (A + 1 - (A - 1) * cos - two),
      A + 1 + (A - 1) * cos + two,
      -2 * (A - 1 + (A + 1) * cos),
      A + 1 + (A - 1) * cos - two,
    )
  }
  return normalized(
    A * (A + 1 + (A - 1) * cos + two),
    -2 * A * (A - 1 + (A + 1) * cos),
    A * (A + 1 + (A - 1) * cos - two),
    A + 1 - (A - 1) * cos + two,
    2 * (A - 1 - (A + 1) * cos),
    A + 1 - (A - 1) * cos - two,
  )
}

/** Biquad::magnitude_db */
function magnitudeDb([b0, b1, b2, a1, a2]: Coeffs, sr: number, hz: number): number {
  const w = (2 * Math.PI * hz) / Math.max(1, sr)
  const [c1, s1, c2, s2] = [Math.cos(w), Math.sin(w), Math.cos(2 * w), Math.sin(2 * w)]
  const nr = b0 + b1 * c1 + b2 * c2
  const ni = -(b1 * s1 + b2 * s2)
  const dr = 1 + a1 * c1 + a2 * c2
  const di = -(a1 * s1 + a2 * s2)
  const power = (nr * nr + ni * ni) / Math.max(1e-30, dr * dr + di * di)
  return 10 * Math.log10(Math.max(1e-30, power))
}

const proportionalQ = (gainDb: number) => 0.7 + Math.min(1, Math.abs(gainDb) / 18) * 1.2

/** filter_response_db: the two 24 dB/oct cuts; one parked open is out of the
 *  path and adds nothing. */
function filterResponseDb(e: Editor, sr: number, freqs: number[]): number[] {
  const hpf = e.value('hpfHz')
  const lpf = e.value('lpfHz')
  const stages: Coeffs[] = []
  if (hpf > HPF_OPEN_HZ) stages.push(...BUTTERWORTH_4_Q.map((q) => highPass(sr, hpf, q)))
  if (lpf < LPF_OPEN_HZ) stages.push(...BUTTERWORTH_4_Q.map((q) => lowPass(sr, lpf, q)))
  return freqs.map((hz) => stages.reduce((db, s) => db + magnitudeDb(s, sr, hz), 0))
}

/** eq_response_db */
function eqResponseDb(e: Editor, sr: number, freqs: number[]): number[] {
  const lmGain = e.value('lowMidGainDb')
  const hmGain = e.value('highMidGainDb')
  const bands = [
    shelf(sr, LOW_SHELF_HZ, e.value('lowGainDb'), false),
    peak(sr, e.value('lowMidFreqHz'), lmGain, proportionalQ(lmGain)),
    peak(sr, e.value('highMidFreqHz'), hmGain, proportionalQ(hmGain)),
    shelf(sr, HIGH_SHELF_HZ, e.value('highGainDb'), true),
  ]
  return freqs.map((hz) => bands.reduce((db, s) => db + magnitudeDb(s, sr, hz), 0))
}

function compressorCurveDb(level: number, threshold: number, ratio: number, knee: number): number {
  const over = level - threshold
  const half = knee / 2
  const slope = 1 / Math.max(1, ratio) - 1
  if (over <= -half) return 0
  if (over >= half) return slope * over
  const t = over + half
  return (slope * t * t) / (2 * Math.max(1e-6, knee))
}

/** comp_transfer_db */
const compTransferDb = (e: Editor, input: number) =>
  input + compressorCurveDb(input, e.value('compThresholdDb'), e.value('compRatio'), COMP_KNEE_DB) + e.value('compMakeupDb')

function softOverDb(over: number, knee: number): number {
  const half = knee / 2
  if (over <= -half) return 0
  if (over >= half) return -over
  const t = over + half
  return -(t * t) / (2 * knee)
}

/** limiter_transfer_db */
function limiterTransferDb(e: Editor, input: number): number {
  const ceiling = e.value('limiterCeilingDb')
  return Math.min(ceiling, input + softOverDb(input - ceiling, LIMITER_KNEE_DB))
}

/** drive_curve: the level-matched, biased tanh waveshaper (dsp::saturate). */
function driveCurve(e: Editor, x: number): number {
  const drive = e.value('satDrivePct') * SATURATION_DRIVE_SCALE
  const character = e.value('satCharacterPct') * 0.01
  if (drive <= 1e-4) return x
  const bias = (Math.min(1, Math.max(0, character)) - 0.5) * 2 * SATURATION_BIAS * (1 - Math.exp(-drive))
  const norm = Math.max(1e-6, 0.5 * (Math.tanh(drive + bias) - Math.tanh(bias - drive)))
  return (Math.tanh(x * drive + bias) - Math.tanh(bias)) / norm
}

/** width_of: what the Width module makes of a hard-panned pair. */
function widthOf(e: Editor, left: number, right: number): [number, number] {
  const mid = (left + right) / 2
  const side = ((left - right) / 2) * e.value('widthPct') * 0.01
  return [mid + side, mid - side]
}

// ── Displays ────────────────────────────────────────────────────────────

const HANDLE_HIT = 12
const HANDLE_R = 6
const CUT_TOP_DB = 6
const CUT_FLOOR_DB = -36
const EQ_RANGE_DB = 18
const COMP_RANGE_DB = -60
const LIMIT_RANGE_DB = -24
/** Where the limiter's ceiling handle sits on its input axis. */
const CEILING_HANDLE_DB = -2

type Handle = 'lowCut' | 'highCut' | 'eqLow' | 'eqLowMid' | 'eqHighMid' | 'eqHigh' | 'threshold' | 'ceiling'

/** What double-clicking a handle puts back to its default. */
const HANDLE_PARAM: Record<Handle, string> = {
  lowCut: 'hpfHz',
  highCut: 'lpfHz',
  eqLow: 'lowGainDb',
  eqLowMid: 'lowMidGainDb',
  eqHighMid: 'highMidGainDb',
  eqHigh: 'highGainDb',
  threshold: 'compThresholdDb',
  ceiling: 'limiterCeilingDb',
}

type Plot = [number, number, number, number]

/** The frequency displays' plot: room for the tags above, the axis below. */
const freqPlot = (w: number, h: number): Plot => [0, 26, w, Math.max(1, h - 26 - 18)]

/** Where each of the module's handles sits. */
function handles(e: Editor, code: number, w: number, h: number): [Handle, number, number][] {
  switch (code) {
    case 1: {
      const [x0, y0, pw, ph] = freqPlot(w, h)
      const y = y0 + ((CUT_TOP_DB + 3) / (CUT_TOP_DB - CUT_FLOOR_DB)) * ph
      return [
        ['lowCut', x0 + freqFraction(e.value('hpfHz')) * pw, y],
        ['highCut', x0 + freqFraction(e.value('lpfHz')) * pw, y],
      ]
    }
    case 2: {
      const [x0, y0, pw, ph] = freqPlot(w, h)
      const x = (hz: number) => x0 + freqFraction(hz) * pw
      const y = (db: number) => y0 + ph * 0.5 - (db / EQ_RANGE_DB) * ph * 0.5
      return [
        ['eqLow', x(LOW_SHELF_HZ), y(e.value('lowGainDb'))],
        ['eqLowMid', x(e.value('lowMidFreqHz')), y(e.value('lowMidGainDb'))],
        ['eqHighMid', x(e.value('highMidFreqHz')), y(e.value('highMidGainDb'))],
        ['eqHigh', x(HIGH_SHELF_HZ), y(e.value('highGainDb'))],
      ]
    }
    case 3: {
      const [x0, y0, pw, ph] = transferPlot(w, h)
      const at = (db: number) => (db - COMP_RANGE_DB) / -COMP_RANGE_DB
      const threshold = e.value('compThresholdDb')
      const out = Math.min(0, Math.max(COMP_RANGE_DB, compTransferDb(e, threshold)))
      return [['threshold', x0 + at(threshold) * pw, y0 + ph - at(out) * ph]]
    }
    case 6: {
      const [x0, y0, pw, ph] = transferPlot(w, h)
      const at = (db: number) => (db - LIMIT_RANGE_DB) / -LIMIT_RANGE_DB
      return [['ceiling', x0 + at(CEILING_HANDLE_DB) * pw, y0 + ph - at(e.value('limiterCeilingDb')) * ph]]
    }
    default:
      return []
  }
}

/** The handle within reach of `(x, y)`, nearest first. */
function hitHandle(e: Editor, code: number, w: number, h: number, x: number, y: number): Handle | null {
  let best: Handle | null = null
  let nearest = HANDLE_HIT
  for (const [handle, hx, hy] of handles(e, code, w, h)) {
    const distance = Math.hypot(hx - x, hy - y)
    if (distance <= nearest) {
      nearest = distance
      best = handle
    }
  }
  return best
}

const round = (v: number, step: number) => Math.round(v / step) * step
const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v))

/** A handle dragged to `(x, y)`, as the native `drag_to` maps it. Each id is
 *  its own coalesced edit, so a mid band's frequency and gain move together. */
function dragHandle(e: Editor, handle: Handle, w: number, h: number, x: number, y: number) {
  switch (handle) {
    case 'lowCut':
    case 'highCut': {
      const [x0, , pw] = freqPlot(w, h)
      const hz = freqAtFraction((x - x0) / pw)
      if (handle === 'lowCut') e.set('hpfHz', round(clamp(hz, 20, 500), 1))
      else e.set('lpfHz', round(clamp(hz, 1_000, 20_000), 10))
      return
    }
    case 'eqLow':
    case 'eqLowMid':
    case 'eqHighMid':
    case 'eqHigh': {
      const [x0, y0, pw, ph] = freqPlot(w, h)
      const gain = round(clamp(((y0 + ph * 0.5 - y) / (ph * 0.5)) * EQ_RANGE_DB, -EQ_RANGE_DB, EQ_RANGE_DB), 0.1)
      const hz = freqAtFraction((x - x0) / pw)
      if (handle === 'eqLow') e.set('lowGainDb', gain)
      else if (handle === 'eqHigh') e.set('highGainDb', gain)
      else if (handle === 'eqLowMid') {
        e.set('lowMidGainDb', gain)
        e.set('lowMidFreqHz', round(clamp(hz, 80, 2_000), 1))
      } else {
        e.set('highMidGainDb', gain)
        e.set('highMidFreqHz', round(clamp(hz, 500, 12_000), 1))
      }
      return
    }
    case 'threshold': {
      const [x0, , pw] = transferPlot(w, h)
      const db = COMP_RANGE_DB - ((x - x0) / pw) * COMP_RANGE_DB
      e.set('compThresholdDb', round(clamp(db, COMP_RANGE_DB, 0), 0.1))
      return
    }
    case 'ceiling': {
      const [, y0, , ph] = transferPlot(w, h)
      const db = (LIMIT_RANGE_DB * (y - y0)) / ph
      e.set('limiterCeilingDb', round(clamp(db, -12, 0), 0.1))
    }
  }
}

let disabledInk: string | null = null
let warningInk: string | null = null
function extraTokens(): [string, string] {
  if (!disabledInk || !warningInk) {
    const s = getComputedStyle(document.documentElement)
    disabledInk = s.getPropertyValue('--text-disabled').trim()
    warningInk = s.getPropertyValue('--warning').trim()
  }
  return [disabledInk, warningInk]
}

const signedDb = (db: number) => `${db >= 0 ? '+' : '-'}${Math.abs(db).toFixed(0)}`

/** paint_module: the selected module's picture, its live spectrum (cuts and
 *  EQ), and its handles. */
function paintModule(ctx: Ctx, w: number, h: number, e: Editor, code: number, running: boolean, held: Handle | null) {
  const c = colors()
  const [disabled, warning] = extraTokens()
  const ink = c.text
  const faint = c.textFaint
  const accent = running ? c.accent : disabled
  const size = DENSE_CAPTION
  if (code === 1 || code === 2) {
    const [x0, y0, pw, ph] = freqPlot(w, h)
    paintSpectrum(ctx, e.live, [x0, y0, pw, ph])
    for (const [hz, text] of [
      [50, '50'],
      [100, '100'],
      [200, '200'],
      [500, '500'],
      [1_000, '1k'],
      [2_000, '2k'],
      [5_000, '5k'],
      [10_000, '10k'],
    ] as [number, string][]) {
      const x = x0 + freqFraction(hz) * pw
      const major = hz === 100 || hz === 1_000 || hz === 10_000
      rect(ctx, x, y0, 1, ph, alpha(ink, major ? 0.09 : 0.045))
      label(ctx, text, size, faint, x, y0 + ph + 3, 'center')
    }
    const freqs = Array.from({ length: 160 }, (_, i) => freqAtFraction(i / 159))
    const yOf =
      code === 1
        ? (db: number) => y0 + clamp((CUT_TOP_DB - db) / (CUT_TOP_DB - CUT_FLOOR_DB), 0, 1) * ph
        : (db: number) => y0 + ph * 0.5 - clamp(db / EQ_RANGE_DB, -1, 1) * ph * 0.5
    const curve = code === 1 ? filterResponseDb(e, e.sampleRate, freqs) : eqResponseDb(e, e.sampleRate, freqs)
    for (const db of code === 1 ? [0, -12, -24] : [12, 6, 0, -6, -12]) {
      const y = yOf(db)
      rect(ctx, x0, y, pw, 1, alpha(ink, db === 0 ? 0.12 : 0.05))
      label(ctx, signedDb(db), size, faint, x0 + 4, y - 13)
    }
    const points = freqs.map((hz, i): [number, number] => [x0 + freqFraction(hz) * pw, yOf(curve[i])])
    if (code === 2) area(ctx, points, yOf(0), alpha(accent, 0.12))
    line(ctx, points, 2, accent)
  } else if (code === 3 || code === 6) {
    const range = code === 3 ? COMP_RANGE_DB : LIMIT_RANGE_DB
    const [x0, y0, pw, ph] = transferPlot(w, h)
    const at = (db: number) => (clamp(db, range, 0) - range) / -range
    const step = code === 3 ? 12 : 6
    for (let db = range; db <= 0.01; db += step) {
      rect(ctx, x0 + at(db) * pw, y0, 1, ph, alpha(ink, 0.06))
      rect(ctx, x0, y0 + ph - at(db) * ph, pw, 1, alpha(ink, 0.06))
      if (db < 0 && db > range) {
        const text = db.toFixed(0)
        label(ctx, text, size, faint, x0 + at(db) * pw, y0 + ph + 4, 'center')
        label(ctx, text, size, faint, x0 - 6, y0 + ph - at(db) * ph - 6, 'right')
      }
    }
    dashed(ctx, [x0, y0 + ph], [x0 + pw, y0], 1, alpha(ink, 0.22))
    if (code === 6) {
      const y = y0 + ph - at(e.value('limiterCeilingDb')) * ph
      dashed(ctx, [x0, y], [x0 + pw, y], 1, alpha(warning, 0.8))
    }
    const points = Array.from({ length: 121 }, (_, i): [number, number] => {
      const input = range - (range * i) / 120
      const output = code === 3 ? compTransferDb(e, input) : limiterTransferDb(e, input)
      return [x0 + at(input) * pw, y0 + ph - at(output) * ph]
    })
    line(ctx, points, 2, accent)
  } else if (code === 4) {
    const [x0, y0, pw, ph] = transferPlot(w, h)
    const side = Math.min(pw, ph)
    const [cx0, cy0] = [x0 + (pw - side) / 2, y0 + (ph - side) / 2]
    const span = 1.2
    const map = (x: number, y: number): [number, number] => [
      cx0 + ((x + span) / (2 * span)) * side,
      cy0 + side - ((y + span) / (2 * span)) * side,
    ]
    rect(ctx, cx0, cy0 + side / 2, side, 1, alpha(ink, 0.12))
    rect(ctx, cx0 + side / 2, cy0, 1, side, alpha(ink, 0.12))
    dashed(ctx, map(-span, -span), map(span, span), 1, alpha(ink, 0.22))
    const points = Array.from({ length: 161 }, (_, i) => {
      const x = -span + (2 * span * i) / 160
      return map(x, clamp(driveCurve(e, x), -span, span))
    })
    line(ctx, points, 2, accent)
  } else if (code === 5) {
    const centre: [number, number] = [w / 2, h * 0.55]
    const reach = Math.max(10, w / 2 - 40) / 2
    const [left, right] = widthOf(e, 1, -1)
    for (const unity of [-1, 1]) {
      const x = centre[0] + unity * reach
      dashed(ctx, [x, centre[1] - 24], [x, centre[1] + 24], 1, alpha(ink, 0.25))
    }
    label(ctx, 'L', size, faint, centre[0] - reach, centre[1] - 40, 'center')
    label(ctx, 'R', size, faint, centre[0] + reach, centre[1] - 40, 'center')
    const lx = centre[0] - left * reach
    const rx = centre[0] - right * reach
    line(ctx, [[lx, centre[1]], [rx, centre[1]]], 2, accent)
    dot(ctx, lx, centre[1], 6, accent)
    dot(ctx, rx, centre[1], 6, alpha(accent, 0.7))
    label(ctx, 'A hard-panned pair · dashed = unchanged', size, faint, centre[0], h - 18, 'center')
  }
  for (const [handle, x, y] of handles(e, code, w, h)) {
    const r = handle === held ? HANDLE_R + 1 : HANDLE_R
    dot(ctx, x, y, r + 2, c.canvas)
    dot(ctx, x, y, r, running ? c.accent : disabled)
  }
}

function describeWidth(pct: number): string {
  if (pct < 0.5) return 'Mono'
  if (Math.abs(pct - 100) < 0.5) return 'As the source'
  return pct > 100 ? 'Wider than the source' : 'Narrower than the source'
}

function legendOf(e: Editor, code: number): [string, string] {
  switch (code) {
    case 1:
      return ['Response', `${e.value('hpfHz').toFixed(0)} Hz – ${(e.value('lpfHz') / 1000).toFixed(1)} kHz`]
    case 2:
      return ['Response', 'Shelves at 100 Hz and 10 kHz']
    case 3:
      return ['Transfer', `${e.value('compThresholdDb').toFixed(1)} dB · ${e.value('compRatio').toFixed(1)}:1`]
    case 4:
      return ['Curve', 'In → Out']
    case 5:
      return ['Image', describeWidth(e.value('widthPct'))]
    default:
      return ['Transfer', `Ceiling ${e.value('limiterCeilingDb').toFixed(1)} dB`]
  }
}

// ── The panel ───────────────────────────────────────────────────────────

/** A rack row's height plus the gap between rows: the pitch a dragged row's
 *  drop position is read at. */
const ROW_PITCH = 44 + 4

function MixStation(props: { editor: Editor }) {
  const { editor } = props
  const rack = rackOf(editor)
  const [picked, setPicked] = useState<number | null>(null)
  const selected = picked !== null && rack.some((s) => s.code === picked) ? picked : (rack[0]?.code ?? null)

  const write = (order: number[], extra: Record<string, number> = {}) => editor.setMany({ ...slotsFor(order), ...extra })
  const add = (code: number) => {
    const m = moduleOf(code)
    const order = rack.map((s) => s.code)
    if (!m || order.includes(code) || order.length >= SLOTS) return
    write([...order, code], { [m.enabled]: 1 })
    setPicked(code)
  }
  const remove = (code: number) => {
    const m = moduleOf(code)
    if (!m) return
    const order = rack.map((s) => s.code).filter((c) => c !== code)
    write(order, { [m.enabled]: 0 })
    setPicked(order[0] ?? null)
  }
  const move = (code: number, target: number) => {
    const order = rack.map((s) => s.code)
    const from = order.indexOf(code)
    if (from < 0) return
    order.splice(from, 1)
    order.splice(Math.min(target, order.length), 0, code)
    write(order)
  }

  return (
    <EditorShell editor={editor} title="MixStation" subtitle={`Channel strip rack · ${rack.length} / ${SLOTS}`}>
      <div className="ms-main">
        <Chain editor={editor} rack={rack} selected={selected} onSelect={setPicked} onAdd={add} onMove={move} />
        {selected === null ? (
          <div className="ms-column">
            <div className="ms-panel ms-empty">
              <span className="ms-empty-title">No modules in the chain</span>
              <span className="ms-hint">Add one from the signal path on the left.</span>
            </div>
          </div>
        ) : (
          <ModuleEditor
            editor={editor}
            code={selected}
            position={rack.findIndex((s) => s.code === selected)}
            onRemove={() => remove(selected)}
          />
        )}
        <div className="ms-panel ms-io">
          <ParamKnob editor={editor} id="inputTrimDb" size={KNOB + 6} />
          <LiveCanvas
            className="ms-meters"
            draw={(ctx, w, h) =>
              paintMeters(
                ctx,
                w,
                h,
                editor.live,
                [{ kind: 'input' }, { kind: 'reduction', caption: 'GR', sign: '−' }, { kind: 'output' }],
                editor.bypassed,
              )
            }
          />
          <ParamKnob editor={editor} id="outputTrimDb" size={KNOB + 6} />
        </div>
      </div>
    </EditorShell>
  )
}

/** An on/off pill. */
function Pill(props: { on: boolean; text: [string, string]; onClick: () => void }) {
  return (
    <button type="button" className={`ms-pill${props.on ? ' on' : ''}`} aria-pressed={props.on} onClick={props.onClick}>
      {props.on ? props.text[0] : props.text[1]}
    </button>
  )
}

function FlowMarker(props: { text: string }) {
  return (
    <div className="ms-flow">
      <span />
      {props.text}
    </div>
  )
}

/** The signal path: the rack rows, the add menu, IN and OUT. */
function Chain(props: {
  editor: Editor
  rack: Stage[]
  selected: number | null
  onSelect: (code: number) => void
  onAdd: (code: number) => void
  onMove: (code: number, target: number) => void
}) {
  const { editor, rack } = props
  const list = useRef<HTMLDivElement>(null)
  const [drag, setDrag] = useState<{ code: number; target: number } | null>(null)
  const power = !editor.bypassed

  const targetAt = (clientY: number) => {
    const top = list.current?.getBoundingClientRect().top ?? 0
    const row = Math.floor((clientY - top) / ROW_PITCH)
    return clamp(row, 0, Math.max(1, rack.length) - 1)
  }
  const grab = (code: number, position: number) => (e: PointerEvent<HTMLButtonElement>) => {
    if (e.button !== 0) return
    e.currentTarget.setPointerCapture(e.pointerId)
    setDrag({ code, target: position })
    props.onSelect(code)
  }
  const follow = (e: PointerEvent<HTMLButtonElement>) => {
    if (!drag) return
    const target = targetAt(e.clientY)
    if (target !== drag.target) setDrag({ ...drag, target })
  }
  const drop = () => {
    if (!drag) return
    props.onMove(drag.code, drag.target)
    setDrag(null)
  }
  // The grip also moves its row from the keyboard, one place at a time.
  const nudge = (code: number, position: number) => (e: KeyboardEvent<HTMLButtonElement>) => {
    const delta = e.key === 'ArrowUp' ? -1 : e.key === 'ArrowDown' ? 1 : 0
    if (delta === 0) return
    e.preventDefault()
    const target = position + delta
    if (target >= 0 && target < rack.length) props.onMove(code, target)
  }

  return (
    <div className="ms-panel ms-chain">
      <div className="ms-chain-head">
        <span className="ms-caption">SIGNAL PATH</span>
        <span className="ms-hint">
          {rack.length} / {SLOTS}
        </span>
      </div>
      <FlowMarker text="IN" />
      <div ref={list} className="ms-rack">
        {rack.map((stage, position) => {
          const m = moduleOf(stage.code)!
          const on = editor.flag(m.enabled)
          const held = drag?.code === stage.code
          const dropHere = drag !== null && drag.code !== stage.code && drag.target === position
          const className = [
            'ms-row',
            props.selected === stage.code ? 'selected' : '',
            held || dropHere ? 'target' : '',
          ]
            .filter(Boolean)
            .join(' ')
          return (
            <div key={stage.code} className={className} onClick={() => props.onSelect(stage.code)}>
              <button
                type="button"
                className="ms-grip"
                title="Drag to move it in the chain (or use the arrow keys)"
                aria-label={`Move ${m.name}`}
                onPointerDown={grab(stage.code, position)}
                onPointerMove={follow}
                onPointerUp={drop}
                onPointerCancel={() => setDrag(null)}
                onKeyDown={nudge(stage.code, position)}
              >
                ⠿
              </button>
              <div className="ms-row-body">
                <span className={`ms-row-name${on && power ? ' live' : ''}`}>
                  {position + 1}
                  {' '}
                  {m.name}
                </span>
                <LiveCanvas
                  className="ms-stage"
                  draw={(ctx, w, h) => {
                    const frame = editor.live.frame
                    // A row reads its rack position's levels only while its
                    // module processes, as the native row does.
                    const reading = frame && !editor.bypassed && editor.flag(m.enabled)
                    paintStageBars(
                      ctx,
                      0,
                      0,
                      w,
                      h,
                      reading ? (frame.slot_in_peak[stage.slot] ?? 0) : 0,
                      reading ? (frame.slot_out_peak[stage.slot] ?? 0) : 0,
                    )
                  }}
                />
              </div>
              <Pill on={on} text={['On', 'Off']} onClick={() => editor.set(m.enabled, on ? 0 : 1)} />
            </div>
          )
        })}
      </div>
      {rack.length === 0 && <div className="ms-hint ms-rack-empty">The rack is empty. Add a module to start the chain.</div>}
      <AddModule rack={rack} onAdd={props.onAdd} />
      <FlowMarker text="OUT" />
      <div className="ms-spacer" />
      <span className="ms-hint">Drag a row's grip to move it in the chain.</span>
    </div>
  )
}

/** "+ Add module" and its menu of the modules not yet in the rack. */
function AddModule(props: { rack: Stage[]; onAdd: (code: number) => void }) {
  const [open, setOpen] = useState(false)
  const box = useRef<HTMLDivElement>(null)
  useEffect(() => {
    if (!open) return
    const away = (e: globalThis.PointerEvent) => {
      if (!box.current?.contains(e.target as Node)) setOpen(false)
    }
    const escape = (e: globalThis.KeyboardEvent) => e.key === 'Escape' && setOpen(false)
    document.addEventListener('pointerdown', away)
    document.addEventListener('keydown', escape)
    return () => {
      document.removeEventListener('pointerdown', away)
      document.removeEventListener('keydown', escape)
    }
  }, [open])
  const missing = MODULES.filter((m) => !props.rack.some((s) => s.code === m.code))
  return (
    <div ref={box} className="ms-add-box">
      <button
        type="button"
        className={`ms-add${missing.length === 0 ? ' full' : ''}`}
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        +{' '}Add module
      </button>
      {open && (
        <div className="ms-menu" role="menu">
          <div className="ms-menu-head">Add module</div>
          {missing.length === 0 && <div className="ms-menu-item disabled">All six modules are in the chain</div>}
          {missing.map((m) => (
            <button
              key={m.code}
              type="button"
              role="menuitem"
              className="ms-menu-item"
              onClick={() => {
                setOpen(false)
                props.onAdd(m.code)
              }}
            >
              {m.name} — {m.hint}
            </button>
          ))}
        </div>
      )}
    </div>
  )
}

/** The selected module: its title, display and controls. */
function ModuleEditor(props: { editor: Editor; code: number; position: number; onRemove: () => void }) {
  const { editor, code } = props
  const m = moduleOf(code)!
  const on = editor.flag(m.enabled)
  const running = on && !editor.bypassed
  const held = useRef<Handle | null>(null)
  const [title, legend] = legendOf(editor, code)
  const reset = () => {
    const values: Record<string, number> = {}
    for (const id of [...m.knobs, m.trim]) values[id] = editor.defaultOf(id)
    editor.setMany(values)
  }
  const why = editor.bypassed ? 'MixStation is bypassed' : !on ? `${m.name} is bypassed` : null
  const size = m.knobs.length > 4 ? KNOB : KNOB + 6
  const boxSize = (e: { currentTarget: HTMLElement }): [number, number] => [
    e.currentTarget.clientWidth,
    e.currentTarget.clientHeight,
  ]

  return (
    <div className="ms-column">
      <div className="ms-module-head">
        <div className="ms-module-title">
          <strong>
            {props.position + 1}
            {' '}
            {m.name}
          </strong>
          <span className="ms-hint">{m.hint}</span>
        </div>
        <Pill on={on} text={['On', 'Bypassed']} onClick={() => editor.set(m.enabled, on ? 0 : 1)} />
        <button type="button" className="button ghost" onClick={reset}>
          Reset
        </button>
        <button type="button" className="button ghost" onClick={props.onRemove}>
          Remove
        </button>
      </div>
      <LiveCanvas
        className="ms-display"
        draw={(ctx, w, h) => paintModule(ctx, w, h, editor, code, running, held.current)}
        onPointerDown={(x, y, e) => {
          const [w, h] = boxSize(e)
          held.current = hitHandle(editor, code, w, h, x, y)
        }}
        onPointerMove={(x, y, e) => {
          const [w, h] = boxSize(e)
          if (held.current) dragHandle(editor, held.current, w, h, x, y)
          else e.currentTarget.style.cursor = hitHandle(editor, code, w, h, x, y) ? 'grab' : ''
        }}
        onPointerUp={() => {
          held.current = null
        }}
        onDoubleClick={(x, y, e) => {
          const [w, h] = boxSize(e)
          const handle = hitHandle(editor, code, w, h, x, y)
          if (handle) editor.set(HANDLE_PARAM[handle], editor.defaultOf(HANDLE_PARAM[handle]))
        }}
      >
        <DisplayTag>{title}</DisplayTag>
        <DisplayTag right>{legend}</DisplayTag>
        {editor.bypassed && (
          <div className="ms-notice">
            <span>Bypassed — MixStation passes audio through unchanged</span>
          </div>
        )}
      </LiveCanvas>
      <div className="ms-panel ms-controls">
        <span className="ms-caption">CONTROLS</span>
        {/* The shell already dims every knob while the strip is bypassed. */}
        <div className={`ms-knobs${running || editor.bypassed ? '' : ' idle'}`}>
          {m.knobs.map((id) => (
            <ParamKnob key={id} editor={editor} id={id} size={size} />
          ))}
          <div className="ms-divider" />
          <ParamKnob editor={editor} id={m.trim} size={KNOB} />
        </div>
        {why && <span className="ms-hint">{why}</span>}
      </div>
    </div>
  )
}

export const editors: Record<string, EditorComponent> = {
  mixstation: MixStation,
}
