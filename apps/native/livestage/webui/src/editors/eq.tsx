// Editors for EQ-Z8 and EQ-ZX: a port of the native editor both share
// (components/eq_model.rs, eq_graph.rs, eq_panel.rs, eq_window.rs).
//
// The two EQs keep their own wire tables; this file reads and writes them
// through one band shape, `Band`, as eq_model.rs does, so the graph, the band
// strip and the band editor are written once. Every curve is the DSP crates'
// own response — the same biquad coefficients (builtin_dsp_core's
// `make_eq_coefficients`, the `biquad` crate's cookbook) at the engine's
// sample rate, EQ-ZX's cuts as their Butterworth cascades.
//
// Graph gestures, as in Studio: drag a numbered node (Shift fine, Alt keeps
// its gain), double-click empty graph to add a band, double-click a node to
// flatten it, wheel over a node for Q (a cut's slope on EQ-ZX), hold the
// right button on a node to hear it alone while moving it, Alt-click a node
// to keep listening. Escape ends an audition; so does closing.

import { useEffect, useMemo, useRef, useState } from 'react'
import type { KeyboardEvent as ReactKeyboardEvent, MouseEvent as ReactMouseEvent, PointerEvent as ReactPointerEvent } from 'react'
import { Knob } from '../controls.tsx'
import type { Editor, EditorComponent } from './kit.tsx'
import { Card, Check, Choice, EditorShell, KNOB, LiveCanvas, Row, Toggle } from './kit.tsx'
import { SPECTRUM_CEIL_DB, SPECTRUM_FLOOR_DB, SPECTRUM_MAX_HZ, SPECTRUM_MIN_HZ } from './live.ts'
import type { Ctx } from './paint.ts'
import { alpha, colors, freqAtFraction, freqFraction, line, rect } from './paint.ts'
import './eq.css'

// ── The model (eq_model.rs) ─────────────────────────────────────────────

/** The ranges both DSPs clamp to (equzx::ipc; EQ-Z8 hard-codes the same). */
const FREQ_MIN = 20
const FREQ_MAX = 20000
const GAIN_MIN_DB = -18
const GAIN_MAX_DB = 18
const Q_MIN = 0.1
const Q_MAX = 12
const THRESHOLD_MIN_DB = -60
const THRESHOLD_MAX_DB = 0
const RANGE_MIN_DB = -24
const RANGE_MAX_DB = 24
const ATTACK_MIN_MS = 0.1
const ATTACK_MAX_MS = 500
const RELEASE_MIN_MS = 1
const RELEASE_MAX_MS = 5000
const OUTPUT_MIN_DB = -24
const OUTPUT_MAX_DB = 12
const SLOPES = [12, 24, 36, 48, 72, 96] as const
/** The slope an EQ-Z8 cut has: one second-order section. */
const Z8_SLOPE = 12

type Kind = 'z8' | 'zx'
type Shape = 'lowCut' | 'lowShelf' | 'bell' | 'notch' | 'bandPass' | 'highShelf' | 'highCut'
/** Stereo, mid, side: EQ-ZX's `BandChannel` wire values. */
type Placement = 0 | 1 | 2

interface KindInfo {
  title: string
  subtitle: string
  slots: number
  /** The shapes a band can take, in the editor's order. */
  shapes: Shape[]
  /** Shapes by wire value (`BandType::to_wire`). */
  wire: Shape[]
  /** EQ-ZX: mid/side placement, cut slopes, dynamics below the threshold. */
  zx: boolean
  /** EQ-Z8's eight bands are always there, and any edit to a switched-off
   *  one switches it on — moving a band you cannot hear would be an edit
   *  with no result. EQ-ZX bands are created and removed instead. */
  editSwitchesOn: boolean
}

const KINDS: Record<Kind, KindInfo> = {
  z8: {
    title: 'EQ-Z8',
    subtitle: '8-band dynamic EQ',
    slots: 8,
    shapes: ['lowCut', 'lowShelf', 'bell', 'notch', 'highShelf', 'highCut'],
    wire: ['lowCut', 'lowShelf', 'bell', 'notch', 'highShelf', 'highCut'],
    zx: false,
    editSwitchesOn: true,
  },
  zx: {
    title: 'EQ-ZX',
    subtitle: '24-band dynamic mid/side EQ',
    slots: 24,
    shapes: ['lowCut', 'lowShelf', 'bell', 'notch', 'bandPass', 'highShelf', 'highCut'],
    wire: ['lowCut', 'lowShelf', 'bell', 'notch', 'bandPass', 'highShelf', 'highCut'],
    zx: true,
    editSwitchesOn: false,
  },
}

const SHAPE_SHORT: Record<Shape, string> = {
  lowCut: 'LC',
  lowShelf: 'LS',
  bell: 'BELL',
  notch: 'NOTCH',
  bandPass: 'BP',
  highShelf: 'HS',
  highCut: 'HC',
}

/** The builtin_dsp_core filter kind one section of a shape runs. */
const FILTER: Record<Shape, FilterKind> = {
  lowCut: 'highpass',
  lowShelf: 'lowshelf',
  bell: 'bell',
  notch: 'notch',
  bandPass: 'bandpass',
  highShelf: 'highshelf',
  highCut: 'lowpass',
}

const PLACEMENT_LABEL = ['Stereo', 'Mid', 'Side'] as const
const PLACEMENT_SHORT = ['ST', 'M', 'S'] as const

const hasGain = (shape: Shape) => shape === 'lowShelf' || shape === 'bell' || shape === 'highShelf'
const isCut = (shape: Shape) => shape === 'lowCut' || shape === 'highCut'
/** EQ-Z8's single-section cuts take their Q as resonance; EQ-ZX's cuts are
 *  Butterworth, set by the slope. */
const usesQ = (kind: Kind, shape: Shape) => kind === 'z8' || !isCut(shape)
/** Whether a band placed at `placement` is heard in a view of `view`. */
const heardIn = (placement: Placement, view: Placement) => view === 0 || placement === 0 || placement === view

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v))

interface Band {
  active: boolean
  shape: Shape
  freq: number
  gainDb: number
  q: number
  /** dB/oct; meaningful for an EQ-ZX cut only. */
  slope: number
  placement: Placement
  dynamic: boolean
  /** Engages as the level falls below the threshold (EQ-ZX only). */
  dynBelow: boolean
  thresholdDb: number
  rangeDb: number
  attackMs: number
  releaseMs: number
}

const dynamicsLive = (b: Band) => b.dynamic && hasGain(b.shape)
/** The gain a node sits at: its own on a shape with gain, 0 dB otherwise. */
const nodeGain = (b: Band) => (hasGain(b.shape) ? b.gainDb : 0)

const idOf = (index: number, field: string) => `band${index + 1}_${field}`

/** Band `index` as the values `read` gives (the insert's, or the defaults). */
function readBand(kind: Kind, index: number, read: (id: string) => number): Band {
  const v = (field: string) => read(idOf(index, field))
  const zx = KINDS[kind].zx
  const type = Math.round(v('type'))
  const channel = zx ? Math.round(v('channel')) : 0
  return {
    active: v('enabled') >= 0.5,
    // `BandType::from_wire`: anything unknown is a bell.
    shape: KINDS[kind].wire[type] ?? 'bell',
    freq: v('freq'),
    gainDb: v('gainDb'),
    q: v('q'),
    slope: zx ? v('slope') : Z8_SLOPE,
    placement: channel === 1 || channel === 2 ? channel : 0,
    dynamic: v('dynEnabled') >= 0.5,
    dynBelow: zx && Math.round(v('dynMode')) === 1,
    thresholdDb: v('thresholdDb'),
    rangeDb: v('rangeDb'),
    attackMs: v('attackMs'),
    releaseMs: v('releaseMs'),
  }
}

/** `band` as its wire values, in wire order (the type before the dynamics
 *  switch, which the DSP only accepts on a shape with gain). */
function bandWire(kind: Kind, index: number, b: Band): Record<string, number> {
  const zx = KINDS[kind].zx
  const type = KINDS[kind].wire.indexOf(b.shape)
  const out: Record<string, number> = {
    [idOf(index, 'enabled')]: b.active ? 1 : 0,
    // EQ-Z8 has no band pass; `Shape::to_z8` makes it a bell.
    [idOf(index, 'type')]: type < 0 ? 2 : type,
    [idOf(index, 'freq')]: b.freq,
    [idOf(index, 'gainDb')]: b.gainDb,
    [idOf(index, 'q')]: b.q,
  }
  if (zx) {
    out[idOf(index, 'slope')] = b.slope
    out[idOf(index, 'channel')] = b.placement
  }
  out[idOf(index, 'dynEnabled')] = b.dynamic ? 1 : 0
  if (zx) out[idOf(index, 'dynMode')] = b.dynBelow ? 1 : 0
  out[idOf(index, 'thresholdDb')] = b.thresholdDb
  out[idOf(index, 'rangeDb')] = b.rangeDb
  out[idOf(index, 'attackMs')] = b.attackMs
  out[idOf(index, 'releaseMs')] = b.releaseMs
  return out
}

/** equzx::ipc::snap_slope: the nearest realizable slope, the first on a tie. */
function snapSlope(value: number): number {
  let best: number = SLOPES[0]
  for (const slope of SLOPES) if (Math.abs(slope - value) < Math.abs(best - value)) best = slope
  return best
}

/** The plug-in's `sanitize_params` for one band: every value back in the
 *  range the DSP accepts, so the editor never shows one it would pin. */
function sanitize(kind: Kind, b: Band): Band {
  return {
    ...b,
    freq: clamp(b.freq, FREQ_MIN, FREQ_MAX),
    gainDb: clamp(b.gainDb, GAIN_MIN_DB, GAIN_MAX_DB),
    q: clamp(b.q, Q_MIN, Q_MAX),
    slope: kind === 'zx' ? snapSlope(b.slope) : Z8_SLOPE,
    thresholdDb: clamp(b.thresholdDb, THRESHOLD_MIN_DB, THRESHOLD_MAX_DB),
    rangeDb: clamp(b.rangeDb, RANGE_MIN_DB, RANGE_MAX_DB),
    attackMs: clamp(b.attackMs, ATTACK_MIN_MS, ATTACK_MAX_MS),
    releaseMs: clamp(b.releaseMs, RELEASE_MIN_MS, RELEASE_MAX_MS),
    dynamic: b.dynamic && hasGain(b.shape),
  }
}

function sameBand(a: Band, b: Band): boolean {
  const near = (x: number, y: number) => Math.abs(x - y) < 1e-6
  return (
    a.active === b.active &&
    a.shape === b.shape &&
    near(a.freq, b.freq) &&
    near(a.gainDb, b.gainDb) &&
    near(a.q, b.q) &&
    near(a.slope, b.slope) &&
    a.placement === b.placement &&
    a.dynamic === b.dynamic &&
    a.dynBelow === b.dynBelow &&
    near(a.thresholdDb, b.thresholdDb) &&
    near(a.rangeDb, b.rangeDb) &&
    near(a.attackMs, b.attackMs) &&
    near(a.releaseMs, b.releaseMs)
  )
}

/** What an empty slot holds: EQ-ZX's unused band, and what removing a band
 *  leaves behind (`empty_band`: the defaults' first band). */
const emptyBand = (editor: Editor, kind: Kind) => readBand(kind, 0, editor.defaultOf)

/** Slot `index`'s default values: a double-click on a knob resets to these. */
const defaultBand = (editor: Editor, kind: Kind, index: number) => readBand(kind, index, editor.defaultOf)

/** Whether slot `index` holds a band the editor lists: every EQ-Z8 band; an
 *  EQ-ZX slot once switched on or set to anything but an empty slot's values. */
function inUse(editor: Editor, kind: Kind, index: number): boolean {
  if (kind === 'z8') return index < KINDS.z8.slots
  const band = readBand(kind, index, editor.value)
  return band.active || !sameBand(band, emptyBand(editor, kind))
}

function listedOf(editor: Editor, kind: Kind): number[] {
  return Array.from({ length: KINDS[kind].slots }, (_, i) => i).filter((i) => inUse(editor, kind, i))
}

/** The slot a new band takes: EQ-Z8's first switched-off band, EQ-ZX's first
 *  free slot. */
function freeSlot(editor: Editor, kind: Kind): number | null {
  for (let i = 0; i < KINDS[kind].slots; i++) {
    const free = kind === 'z8' ? !readBand(kind, i, editor.value).active : !inUse(editor, kind, i)
    if (free) return i
  }
  return null
}

function soloOf(editor: Editor, kind: Kind): number | null {
  const s = Math.round(editor.value('soloBand'))
  return s >= 0 && s < KINDS[kind].slots ? s : null
}

/** The next slope a wheel step reaches: `steeper` up the list, otherwise
 *  down it, stopping at either end. */
function stepSlope(slope: number, steeper: boolean): number {
  let index: number = SLOPES.findIndex((s) => Math.abs(s - slope) < 0.5)
  if (index < 0) index = 1
  return SLOPES[steeper ? Math.min(SLOPES.length - 1, index + 1) : Math.max(0, index - 1)]
}

// ── Readouts ────────────────────────────────────────────────────────────

/** Frequency as a band shows it: `1.25k`, `16.5k`, `440`. */
function formatFreq(hz: number): string {
  if (hz >= 10000) return `${(hz / 1000).toFixed(1).replace(/(\.0)+$/, '')}k`
  if (hz >= 1000) return `${(hz / 1000).toFixed(2).replace(/0+$/, '').replace(/\.$/, '')}k`
  return `${Math.round(hz)}`
}

/** A signed decibel value: `+3.5`, `0`, `-12.0`. */
function formatDb(db: number): string {
  if (Math.abs(db) < 0.05) return '0'
  return db > 0 ? `+${db.toFixed(1)}` : db.toFixed(1)
}

function formatMs(ms: number): string {
  if (ms < 10) return `${ms.toFixed(1)} ms`
  if (ms < 1000) return `${Math.round(ms)} ms`
  return `${(ms / 1000).toFixed(2)} s`
}

// ── The filters (builtin_dsp_core + the `biquad` crate) ─────────────────

export type FilterKind = 'bell' | 'lowshelf' | 'highshelf' | 'lowpass' | 'highpass' | 'notch' | 'bandpass'

export interface Coeffs {
  b0: number
  b1: number
  b2: number
  a1: number
  a2: number
}

/** `make_eq_coefficients`: the Audio EQ Cookbook biquad the DSP runs, its
 *  centre clamped to 0.49 × the sample rate and its Q to 0.1–12. Rounded to
 *  f32 at the end, as the DSP stores them. */
export function coefficients(kind: FilterKind, freqHz: number, gainDb: number, q: number, sampleRate: number): Coeffs | null {
  const fs = Math.max(1, sampleRate)
  const f0 = clamp(freqHz, 10, fs * 0.49)
  const qv = clamp(q, 0.1, 12)
  const normalized = f0 / (fs / 2)
  if (normalized >= 1 || normalized < 0) return null
  const omega = Math.PI * normalized
  const s = Math.sin(omega)
  const c = Math.cos(omega)
  const alphaQ = s / (2 * qv)
  let b0: number, b1: number, b2: number, a0: number, a1: number, a2: number
  switch (kind) {
    case 'lowpass':
      ;[b0, b1, b2] = [(1 - c) / 2, 1 - c, (1 - c) / 2]
      ;[a0, a1, a2] = [1 + alphaQ, -2 * c, 1 - alphaQ]
      break
    case 'highpass':
      ;[b0, b1, b2] = [(1 + c) / 2, -(1 + c), (1 + c) / 2]
      ;[a0, a1, a2] = [1 + alphaQ, -2 * c, 1 - alphaQ]
      break
    case 'bandpass':
      ;[b0, b1, b2] = [s / 2, 0, -(s / 2)]
      ;[a0, a1, a2] = [1 + alphaQ, -2 * c, 1 - alphaQ]
      break
    case 'notch':
      ;[b0, b1, b2] = [1, -2 * c, 1]
      ;[a0, a1, a2] = [1 + alphaQ, -2 * c, 1 - alphaQ]
      break
    case 'lowshelf': {
      const a = Math.pow(10, gainDb / 40)
      const k = 2 * alphaQ * Math.sqrt(a)
      b0 = a * (a + 1 - (a - 1) * c + k)
      b1 = 2 * a * (a - 1 - (a + 1) * c)
      b2 = a * (a + 1 - (a - 1) * c - k)
      a0 = a + 1 + (a - 1) * c + k
      a1 = -2 * (a - 1 + (a + 1) * c)
      a2 = a + 1 + (a - 1) * c - k
      break
    }
    case 'highshelf': {
      const a = Math.pow(10, gainDb / 40)
      const k = 2 * alphaQ * Math.sqrt(a)
      b0 = a * (a + 1 + (a - 1) * c + k)
      b1 = -2 * a * (a - 1 + (a + 1) * c)
      b2 = a * (a + 1 + (a - 1) * c - k)
      a0 = a + 1 - (a - 1) * c + k
      a1 = 2 * (a - 1 - (a + 1) * c)
      a2 = a + 1 - (a - 1) * c - k
      break
    }
    case 'bell': {
      const a = Math.pow(10, gainDb / 40)
      ;[b0, b1, b2] = [1 + alphaQ * a, -2 * c, 1 - alphaQ * a]
      ;[a0, a1, a2] = [1 + alphaQ / a, -2 * c, 1 - alphaQ / a]
      break
    }
    default:
      return null
  }
  const f = Math.fround
  return { b0: f(b0 / a0), b1: f(b1 / a0), b2: f(b2 / a0), a1: f(a1 / a0), a2: f(a2 / a0) }
}

/** `biquad_response_db`: the magnitude of H(e^jw) in dB. */
export function responseDb(c: Coeffs, hz: number, sampleRate: number): number {
  const w = (2 * Math.PI * hz) / Math.max(1, sampleRate)
  const [cos1, sin1, cos2, sin2] = [Math.cos(w), Math.sin(w), Math.cos(2 * w), Math.sin(2 * w)]
  const numRe = c.b0 + c.b1 * cos1 + c.b2 * cos2
  const numIm = -(c.b1 * sin1 + c.b2 * sin2)
  const denRe = 1 + c.a1 * cos1 + c.a2 * cos2
  const denIm = -(c.a1 * sin1 + c.a2 * sin2)
  const power = (numRe * numRe + numIm * numIm) / Math.max(1e-30, denRe * denRe + denIm * denIm)
  return 10 * Math.log10(Math.max(1e-30, power))
}

/** The sections a band runs at `gainDb` (none while it is off): a
 *  Butterworth cascade sized by the slope for an EQ-ZX cut, one section
 *  otherwise (`band_response_at_gain_db` in each crate). */
function sectionsOf(kind: Kind, b: Band, gainDb: number, sampleRate: number): Coeffs[] {
  if (!b.active) return []
  const filter = FILTER[b.shape]
  const qs: number[] = []
  if (kind === 'zx' && isCut(b.shape)) {
    const count = clamp(Math.floor(Math.max(2, Math.round(b.slope / 6)) / 2), 1, 8)
    const order = count * 2
    for (let k = 0; k < count; k++) qs.push(1 / (2 * Math.cos(((2 * k + 1) * Math.PI) / (2 * order))))
  } else {
    qs.push(b.q)
  }
  return qs.map((q) => coefficients(filter, b.freq, gainDb, q, sampleRate)).filter((c): c is Coeffs => c !== null)
}

const sum = (sections: Coeffs[], hz: number, sampleRate: number) =>
  sections.reduce((total, c) => total + responseDb(c, hz, sampleRate), 0)

// ── The graph (eq_graph.rs) ─────────────────────────────────────────────

/** Frequencies the curves are sampled at. */
const CURVE_POINTS = 256
/** The dB spans the graph can show, ± each. */
const DB_RANGES = [6, 12, 18, 30] as const
const DEFAULT_DB_RANGE = 18
/** How near the pointer must be to a node to take it, in pixels. */
const NODE_HIT_RADIUS = 12
/** The whole EQ's fill under its curve: a wash the band curves and the
 *  analyser read through. */
const TOTAL_FILL_ALPHA = 0.12
const FREQ_GRID = [
  20, 30, 40, 50, 60, 70, 80, 90, 100, 200, 300, 400, 500, 600, 700, 800, 900, 1000, 2000, 3000, 4000, 5000,
  6000, 7000, 8000, 9000, 10000, 20000,
]
const FREQ_LABELS: [number, string][] = [
  [30, '30'],
  [60, '60'],
  [100, '100'],
  [300, '300'],
  [600, '600'],
  [1000, '1k'],
  [3000, '3k'],
  [6000, '6k'],
  [10000, '10k'],
]
/** Drag travel under Shift. */
const FINE = 0.25
/** How long a passing word ("All 8 bands are in use") stays up. */
const NOTICE_MS = 2200
/** Two presses this close make a double-click (the native click count),
 *  on a mouse and a touch screen alike. */
const DOUBLE_MS = 400
const DOUBLE_PX = 8

/** Each band's colour, on its node, strip cell and curve: the native
 *  theme's categorical hues, cycling. The accent stays for selection. */
const bandColor = (index: number) => {
  const bands = colors().bands
  return bands[index % bands.length]
}

/** Fraction down the plot (0 top) of `db` on a ±`range` scale. */
const dbFraction = (db: number, range: number) => 0.5 - db / (2 * Math.max(1, range))
const dbAtFraction = (f: number, range: number) => (0.5 - f) * 2 * Math.max(1, range)
const sampleHz = (i: number) => freqAtFraction(i / (CURVE_POINTS - 1))

/** The dB lines a ±`range` grid draws, top to bottom. */
function dbGrid(range: number): number[] {
  const step = range <= 12 ? 3 : 6
  const lines: number[] = []
  for (let db = Math.floor(range / step) * step; db >= -range; db -= step) lines.push(db)
  return lines
}

/** What the graph draws as lines, sampled at CURVE_POINTS frequencies. */
interface Curves {
  /** The whole EQ, for the view shown. */
  total: number[]
  /** Each switched-on band heard in the view, by slot. */
  bands: [number, number[]][]
  /** The selected band with its dynamics fully engaged: where it can move. */
  ghost: [number, number[]] | null
}

interface Model {
  kind: Kind
  bands: Band[]
  /** Each band's sections at its own gain, by slot. */
  sections: Coeffs[][]
  listed: number[]
  outputDb: number
  sampleRate: number
}

/** The whole EQ's response at `hz` for the part of the image `view` shows,
 *  output gain included (`response_db` in each crate). */
function totalAt(m: Model, view: Placement, hz: number): number {
  let total = m.outputDb
  m.bands.forEach((band, i) => {
    const heard = m.kind === 'z8' || view === 0 || band.placement === 0 || band.placement === view
    if (heard) total += sum(m.sections[i], hz, m.sampleRate)
  })
  return total
}

function computeCurves(m: Model, view: Placement, selected: number | null): Curves {
  const points = Array.from({ length: CURVE_POINTS }, (_, i) => sampleHz(i))
  const curve = (sections: Coeffs[]) => points.map((hz) => sum(sections, hz, m.sampleRate))
  const bands = m.listed
    .filter((i) => m.bands[i].active && heardIn(m.bands[i].placement, view))
    .map((i): [number, number[]] => [i, curve(m.sections[i])])
  let ghost: [number, number[]] | null = null
  if (selected !== null) {
    const b = m.bands[selected]
    if (b.active && dynamicsLive(b) && Math.abs(b.rangeDb) >= 0.01 && heardIn(b.placement, view)) {
      ghost = [selected, curve(sectionsOf(m.kind, b, b.gainDb + b.rangeDb, m.sampleRate))]
    }
  }
  return { total: points.map((hz) => totalAt(m, view, hz)), bands, ghost }
}

/** Where band `index`'s node sits, as fractions across and down the plot. */
function nodeFractions(b: Band, range: number): [number, number] {
  return [freqFraction(b.freq), clamp(dbFraction(nodeGain(b), range), 0, 1)]
}

/** The band whose node is under `(x, y)` in a `w`×`h` plot: the nearest
 *  within the hit radius, the selected band winning a tie so a stack of
 *  nodes can still be dragged apart. */
function nodeAt(m: Model, view: Placement, range: number, w: number, h: number, x: number, y: number, selected: number | null) {
  let best: [number, number] | null = null
  for (const i of m.listed) {
    if (!heardIn(m.bands[i].placement, view)) continue
    const [fx, fy] = nodeFractions(m.bands[i], range)
    let distance = Math.hypot(fx * w - x, fy * h - y)
    if (distance > NODE_HIT_RADIUS) continue
    if (selected === i) distance -= 0.5
    if (!best || distance < best[1]) best = [i, distance]
  }
  return best ? best[0] : null
}

function paintGraph(
  ctx: Ctx,
  w: number,
  h: number,
  props: {
    curves: Curves
    range: number
    spectrum: Float32Array | null
    showBandCurves: boolean
    selected: number | null
    bypassed: boolean
    hover: number | null
  },
) {
  const c = colors()
  const { range } = props
  for (const hz of FREQ_GRID) {
    const major = FREQ_LABELS.some(([at]) => at === hz)
    rect(ctx, freqFraction(hz) * w, 0, 1, h, alpha(c.text, major ? 0.09 : 0.045))
  }
  for (const db of dbGrid(range)) {
    rect(ctx, 0, dbFraction(db, range) * h, w, 1, alpha(c.text, db === 0 ? 0.18 : 0.09))
  }

  // The analyser, behind the curves: the signal arriving at the insert.
  if (props.spectrum && !props.bypassed) paintAnalyser(ctx, w, h, props.spectrum, c.textSecondary)

  const dim = props.bypassed ? 0.3 : 1
  const zeroY = dbFraction(0, range) * h
  const toPoints = (curve: number[]) =>
    curve.map((db, i): [number, number] => [
      (i / (CURVE_POINTS - 1)) * w,
      clamp(dbFraction(clamp(db, -range * 1.4, range * 1.4), range) * h, 0, h),
    ])
  for (const [index, curve] of props.curves.bands) {
    const selected = props.selected === index
    if (!props.showBandCurves && !selected) continue
    const color = bandColor(index)
    const points = toPoints(curve)
    if (selected) {
      fillToZero(ctx, points, zeroY, alpha(color, 0.14 * dim))
      line(ctx, points, 1.4, alpha(color, 0.9 * dim))
    } else {
      line(ctx, points, 1, alpha(color, 0.38 * dim))
    }
  }
  if (props.curves.ghost) {
    const [index, curve] = props.curves.ghost
    line(ctx, toPoints(curve), 1, alpha(bandColor(index), 0.5 * dim))
  }
  const total = toPoints(props.curves.total)
  fillToZero(ctx, total, zeroY, alpha(c.accent, TOTAL_FILL_ALPHA * dim))
  line(ctx, total, 2, alpha(c.accent, props.bypassed ? 0.35 : 1))

  if (props.hover !== null) rect(ctx, props.hover * w, 0, 1, h, alpha(c.text, 0.22))
}

/** The area between `points` and the 0 dB line. */
function fillToZero(ctx: Ctx, points: [number, number][], zeroY: number, color: string) {
  ctx.beginPath()
  ctx.moveTo(points[0][0], zeroY)
  for (const [x, y] of points) ctx.lineTo(x, y)
  ctx.lineTo(points[points.length - 1][0], zeroY)
  ctx.closePath()
  ctx.fillStyle = color
  ctx.fill()
}

/** The input spectrum as a soft area: each bin at its centre on the
 *  analyser's own log axis, topping out just under the plot's top. */
function paintAnalyser(ctx: Ctx, w: number, h: number, bins: Float32Array, ink: string) {
  const n = Math.max(1, bins.length)
  ctx.beginPath()
  ctx.moveTo(0, h)
  bins.forEach((db, i) => {
    const hz = SPECTRUM_MIN_HZ * Math.pow(SPECTRUM_MAX_HZ / SPECTRUM_MIN_HZ, (i + 0.5) / n)
    const level = clamp((db - SPECTRUM_FLOOR_DB) / (SPECTRUM_CEIL_DB - SPECTRUM_FLOOR_DB), 0, 1) * 0.92
    ctx.lineTo(freqFraction(hz) * w, h - level * h)
  })
  ctx.lineTo(w, h)
  ctx.closePath()
  const gradient = ctx.createLinearGradient(0, 0, 0, h)
  gradient.addColorStop(0, alpha(ink, 0.3))
  gradient.addColorStop(1, alpha(ink, 0.04))
  ctx.fillStyle = gradient
  ctx.fill()
}

// ── The editor (eq_window.rs, eq_panel.rs) ──────────────────────────────

interface Drag {
  band: number
  /** Where the pointer went down, plot space. */
  origin: [number, number]
  /** The node's place then, as fractions across and down the plot. */
  start: [number, number]
  /** A right-button audition: the solo to put back when it ends. */
  audition: boolean
  restore: number | null
}

/** What an editor shows that is not a parameter, per insert while the page
 *  lives (as a native editor window keeps it while open). */
interface View {
  selected: number | null
  /** Escape left nothing selected (EQ-ZX); the next edit picks again. */
  cleared: boolean
  view: Placement
  range: number
  showSpectrum: boolean
  showBandCurves: boolean
  drag: Drag | null
  /** The pointer's place over the plot, as fractions, while it hovers. */
  hover: [number, number] | null
  notice: { text: string; at: number } | null
  lastDown: { at: number; x: number; y: number } | null
}

const views = new Map<number, View>()
function viewOf(insert: number): View {
  let v = views.get(insert)
  if (!v) {
    v = {
      selected: null,
      cleared: false,
      view: 0,
      range: DEFAULT_DB_RANGE,
      showSpectrum: true,
      showBandCurves: true,
      drag: null,
      hover: null,
      notice: null,
      lastDown: null,
    }
    views.set(insert, v)
  }
  return v
}

function EqEditor(props: { editor: Editor; kind: Kind }) {
  const { editor, kind } = props
  const info = KINDS[kind]
  const ui = viewOf(editor.insert)
  const [, setTick] = useState(0)
  const refresh = () => setTick((t) => t + 1)
  const plotRef = useRef<HTMLDivElement>(null)

  const valuesKey = editor.values().join(',')
  const sampleRate = editor.sampleRate
  // Everything a curve or a node reads, rebuilt only when a value moves.
  const model = useMemo((): Model => {
    const bands = Array.from({ length: info.slots }, (_, i) => readBand(kind, i, editor.value))
    return {
      kind,
      bands,
      sections: bands.map((b) => sectionsOf(kind, b, b.gainDb, sampleRate)),
      listed: listedOf(editor, kind),
      outputDb: editor.value('outputDb'),
      sampleRate,
    }
  }, [valuesKey, sampleRate, kind])

  const solo = soloOf(editor, kind)
  const shownView: Placement = info.zx ? ui.view : 0
  const usesMidSide = info.zx && model.bands.some((b) => b.active && b.placement !== 0)
  // EQ-Z8 always has a band in the editor; EQ-ZX starts on its first.
  const selected =
    ui.selected !== null && model.listed.includes(ui.selected)
      ? ui.selected
      : ui.cleared
        ? null
        : (model.listed[0] ?? null)
  const curvesView: Placement = usesMidSide ? ui.view : 0
  const curves = useMemo(
    () => computeCurves(model, curvesView, selected),
    [model, curvesView, selected],
  )

  // Notices clear themselves.
  useEffect(() => {
    if (!ui.notice) return
    const timer = window.setTimeout(() => {
      ui.notice = null
      refresh()
    }, Math.max(0, NOTICE_MS - (performance.now() - ui.notice.at)))
    return () => window.clearTimeout(timer)
  })

  // An audition never outlives the editor that started it.
  const byInsert = useRef(new Map<number, Editor>())
  byInsert.current.set(editor.insert, editor)
  useEffect(() => {
    const insert = editor.insert
    return () => {
      const last = byInsert.current.get(insert)
      if (last && Math.round(last.value('soloBand')) >= 0) last.set('soloBand', -1)
      const v = views.get(insert)
      if (v) v.drag = null
    }
  }, [editor.insert])

  // ── Edits ──

  /** After an edit: a selection that left the list goes, and none picks
   *  the first band listed. */
  const keepSelectionValid = () => {
    const listed = listedOf(editor, kind)
    if (ui.selected !== null && !listed.includes(ui.selected)) ui.selected = null
    if (ui.selected === null) ui.selected = listed[0] ?? null
    ui.cleared = false
  }

  /** Writes `band` into slot `index` as the wire edits that differ: per id
   *  while a gesture runs (each coalesced), as one command otherwise. */
  const writeBand = (index: number, band: Band, live: boolean, extra: Record<string, number> = {}) => {
    const next = { ...bandWire(kind, index, sanitize(kind, band)), ...extra }
    const changed = Object.entries(next).filter(([id, v]) => Math.abs(editor.value(id) - v) > 1e-7)
    if (changed.length === 0) return
    if (live) for (const [id, v] of changed) editor.set(id, v)
    else editor.setMany(Object.fromEntries(changed))
    keepSelectionValid()
  }

  const editBand = (index: number, change: (b: Band) => void, live: boolean) => {
    const before = readBand(kind, index, editor.value)
    const band = { ...before }
    change(band)
    if (sameBand(band, before)) return
    if (info.editSwitchesOn && !before.active && band.active === before.active) band.active = true
    writeBand(index, band, live)
  }

  const setSolo = (band: number | null) => {
    if (soloOf(editor, kind) === band) return
    editor.set('soloBand', band ?? -1)
  }
  const endAudition = () => {
    if (ui.drag) ui.drag.audition = false
    setSolo(null)
  }
  const toggleSolo = (index: number) => setSolo(soloOf(editor, kind) === index ? null : index)

  const toggleBand = (index: number) => {
    const band = readBand(kind, index, editor.value)
    writeBand(index, { ...band, active: !band.active }, false)
  }

  /** Takes band `index` out: EQ-Z8 switches it off, EQ-ZX empties its slot. */
  const removeBand = (index: number) => {
    const band = kind === 'z8' ? readBand(kind, index, editor.value) : emptyBand(editor, kind)
    const extra: Record<string, number> = soloOf(editor, kind) === index ? { soloBand: -1 } : {}
    writeBand(index, { ...band, active: false }, false, extra)
  }

  const select = (index: number) => {
    ui.selected = index
    ui.cleared = false
    refresh()
  }

  const showNotice = (text: string) => {
    ui.notice = { text, at: performance.now() }
    refresh()
  }

  /** A bell where the user asked for one, placed where the view is. */
  const addBand = (freq: number, gain: number) => {
    const slot = freeSlot(editor, kind)
    if (slot === null) {
      showNotice(
        kind === 'z8'
          ? `All ${info.slots} bands are in use — switch one off to add another`
          : `All ${info.slots} bands are in use — remove one to add another`,
      )
      return
    }
    const band: Band = {
      ...emptyBand(editor, kind),
      active: true,
      shape: 'bell',
      freq: clamp(freq, FREQ_MIN, FREQ_MAX),
      gainDb: clamp(gain, GAIN_MIN_DB, GAIN_MAX_DB),
      q: 1,
      dynamic: false,
      placement: info.zx ? shownView : 0,
    }
    ui.selected = slot
    ui.cleared = false
    writeBand(slot, band, false)
  }

  // ── Graph gestures ──

  const pointValue = (x: number, y: number, w: number, h: number): [number, number] => [
    freqAtFraction(x / Math.max(1, w)),
    clamp(dbAtFraction(y / Math.max(1, h), ui.range), GAIN_MIN_DB, GAIN_MAX_DB),
  ]

  const onDown = (x: number, y: number, e: ReactPointerEvent<HTMLDivElement>) => {
    const { clientWidth: w, clientHeight: h } = e.currentTarget
    const node = nodeAt(model, shownView, ui.range, w, h, x, y, selected)
    const now = performance.now()
    const last = ui.lastDown
    const double =
      e.button === 0 && last !== null && now - last.at < DOUBLE_MS && Math.hypot(x - last.x, y - last.y) < DOUBLE_PX
    ui.lastDown = e.button === 0 && !double ? { at: now, x, y } : null
    const startDrag = (band: number, audition: boolean) => {
      ui.selected = band
      ui.cleared = false
      ui.drag = {
        band,
        origin: [x, y],
        start: nodeFractions(readBand(kind, band, editor.value), ui.range),
        audition,
        restore: soloOf(editor, kind),
      }
    }
    if (e.button === 0 && node !== null && e.altKey) {
      // Alt-click: keep listening to this band, or stop.
      ui.selected = node
      ui.cleared = false
      toggleSolo(node)
    } else if (e.button === 0 && node !== null && double) {
      ui.selected = node
      ui.drag = null
      editBand(node, (b) => {
        b.gainDb = 0
        b.q = 1
      }, false)
    } else if (e.button === 0 && node !== null) {
      startDrag(node, false)
    } else if (e.button === 2 && node !== null) {
      // Hold to hear the band alone while moving it; the solo before it
      // comes back on release.
      startDrag(node, true)
      setSolo(node)
    } else if (e.button === 0 && node === null && double) {
      addBand(...pointValue(x, y, w, h))
    }
    refresh()
  }

  const endDrag = () => {
    const drag = ui.drag
    if (!drag) return
    ui.drag = null
    if (drag.audition) setSolo(drag.restore)
    refresh()
  }

  const onMove = (x: number, y: number, e: ReactPointerEvent<HTMLDivElement>) => {
    const { clientWidth: w, clientHeight: h } = e.currentTarget
    const drag = ui.drag
    if (!drag) {
      const fx = x / Math.max(1, w)
      const fy = y / Math.max(1, h)
      ui.hover = fx >= 0 && fx <= 1 && fy >= 0 && fy <= 1 ? [fx, fy] : null
      refresh()
      return
    }
    if (e.buttons === 0) {
      endDrag()
      return
    }
    const scale = e.shiftKey ? FINE : 1
    const dx = ((x - drag.origin[0]) / Math.max(1, w)) * scale
    const dy = ((y - drag.origin[1]) / Math.max(1, h)) * scale
    const freq = freqAtFraction(drag.start[0] + dx)
    const gain = clamp(dbAtFraction(drag.start[1] + dy, ui.range), GAIN_MIN_DB, GAIN_MAX_DB)
    // Alt keeps the gain where it is; an audition moves frequency only.
    const moveGain = !e.altKey && !drag.audition
    ui.hover = null
    editBand(drag.band, (b) => {
      b.freq = freq
      if (moveGain && hasGain(b.shape)) b.gainDb = Math.round(gain * 10) / 10
    }, true)
  }

  const onUp = (_x: number, _y: number, e: ReactPointerEvent<HTMLDivElement>) => {
    const drag = ui.drag
    if (drag && ((e.button === 2 && drag.audition) || (e.button === 0 && !drag.audition))) endDrag()
  }

  // The wheel needs a listener that may cancel the page's scroll, which a
  // React wheel handler (passive) cannot.
  const wheel = useRef<(e: WheelEvent) => void>(() => {})
  wheel.current = (e: WheelEvent) => {
    const box = plotRef.current
    if (!box) return
    const r = box.getBoundingClientRect()
    const band = nodeAt(model, shownView, ui.range, box.clientWidth, box.clientHeight, e.clientX - r.left, e.clientY - r.top, selected) ?? selected
    const up = -e.deltaY
    if (band === null || up === 0) return
    e.preventDefault()
    const shape = readBand(kind, band, editor.value).shape
    if (info.zx && isCut(shape)) {
      // Down steepens a cut, up softens it.
      editBand(band, (b) => {
        b.slope = stepSlope(b.slope, up < 0)
      }, true)
    } else if (usesQ(kind, shape)) {
      const perNotch = e.shiftKey ? 1.03 : 1.15
      const factor = up > 0 ? perNotch : 1 / perNotch
      editBand(band, (b) => {
        b.q = clamp(b.q * factor, Q_MIN, Q_MAX)
      }, true)
    }
    ui.selected = band
    ui.cleared = false
    refresh()
  }
  useEffect(() => {
    const box = plotRef.current
    if (!box) return
    const listener = (e: WheelEvent) => wheel.current(e)
    box.addEventListener('wheel', listener, { passive: false })
    return () => box.removeEventListener('wheel', listener)
  }, [])

  const onKey = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    const target = e.target as HTMLElement
    if (target.closest('input, select, textarea')) return
    const index = selected
    switch (e.key) {
      case 'Escape':
        if (soloOf(editor, kind) !== null) endAudition()
        else if (kind === 'zx' && index !== null) {
          ui.selected = null
          ui.cleared = true
          refresh()
        }
        break
      case 'Delete':
      case 'Backspace':
        if (index !== null) removeBand(index)
        break
      case 'ArrowLeft':
      case 'ArrowRight':
        if (index !== null) {
          const step = e.shiftKey ? 1.01 : 1.05
          const factor = e.key === 'ArrowRight' ? step : 1 / step
          editBand(index, (b) => {
            b.freq *= factor
          }, true)
        }
        break
      case 'ArrowUp':
      case 'ArrowDown':
        if (index !== null && hasGain(readBand(kind, index, editor.value).shape)) {
          const step = e.key === 'ArrowUp' ? 0.5 : -0.5
          editBand(index, (b) => {
            b.gainDb += step
          }, true)
        }
        break
      default:
        return
    }
    e.preventDefault()
  }

  // Switching A/B ends any audition, as Studio's `swap_compare` does. After
  // the shell's own handler (this is the bubble phase), so the snapshot it
  // parks keeps the solo as it was; the button still shows its old state.
  // Presets and Reset need nothing here: they load solo off, as every
  // factory preset and the defaults carry `soloBand` −1 (`preset_applied`).
  const onShellClick = (e: ReactMouseEvent<HTMLDivElement>) => {
    const ab = (e.target as HTMLElement).closest('.pe-ab button')
    if (ab && !ab.classList.contains('on')) endAudition()
  }

  // ── Render ──

  const bypassed = editor.bypassed
  const drag = ui.drag
  const live = editor.live
  const range = ui.range
  const draw = (ctx: Ctx, w: number, h: number) =>
    paintGraph(ctx, w, h, {
      curves,
      range,
      spectrum: ui.showSpectrum ? live.spectrum : null,
      showBandCurves: ui.showBandCurves,
      selected,
      bypassed,
      hover: ui.hover ? ui.hover[0] : null,
    })

  let readout: string | null = null
  if (drag) {
    const b = model.bands[drag.band]
    readout = `Band ${drag.band + 1} · ${formatFreq(b.freq)} Hz`
    if (hasGain(b.shape)) readout += ` · ${formatDb(b.gainDb)} dB`
    if (usesQ(kind, b.shape)) readout += ` · Q ${b.q.toFixed(2)}`
    if (solo !== null) readout += ` · listening to band ${solo + 1}`
  } else if (ui.hover) {
    const hz = freqAtFraction(ui.hover[0])
    readout = `${formatFreq(hz)} Hz · ${formatDb(totalAt(model, shownView, hz))} dB`
  }

  const notice = ui.notice && performance.now() - ui.notice.at < NOTICE_MS ? ui.notice.text : null
  const hint = bypassed
    ? 'Bypassed — the EQ passes audio through unchanged'
    : notice
      ? notice
      : !model.listed.some((i) => model.bands[i].active)
        ? kind === 'z8'
          ? 'Double-click the graph to add a band, or drag a numbered node'
          : 'Double-click the graph to add a band'
        : null

  const nodes = model.listed
    .filter((i) => heardIn(model.bands[i].placement, shownView))
    .map((i) => {
      const b = model.bands[i]
      const [fx, fy] = nodeFractions(b, range)
      const color = bandColor(i)
      const ring = solo === i ? 'var(--state-solo)' : selected === i ? 'var(--text-primary)' : color
      return (
        <span
          key={i}
          className={`eq-node${b.active ? '' : ' off'}`}
          style={{
            left: `${fx * 100}%`,
            top: `${fy * 100}%`,
            background: b.active ? color : 'var(--surface-canvas)',
            color: b.active ? 'var(--surface-canvas)' : color,
            borderColor: ring,
            borderWidth: solo === i || selected === i ? 2 : 1,
            opacity: bypassed ? 0.35 : 1,
          }}
        >
          {i + 1}
        </span>
      )
    })

  const band = selected !== null ? model.bands[selected] : null

  return (
    <div className="eq-root" tabIndex={-1} onKeyDown={onKey} onClick={onShellClick}>
      <EditorShell editor={editor} title={info.title} subtitle={info.subtitle}>
        <div className="eq-toolbar">
          {info.zx && (
            <div className="eq-view">
              <span className="eq-caption">VIEW</span>
              <Choice
                value={ui.view}
                options={([0, 1, 2] as const).map((p): [Placement, string] => [p, PLACEMENT_LABEL[p]])}
                onChange={(v) => {
                  ui.view = v
                  refresh()
                }}
              />
            </div>
          )}
          <span className="spacer" />
          <Check
            label="Analyser"
            checked={ui.showSpectrum}
            onChange={(on) => {
              ui.showSpectrum = on
              refresh()
            }}
          />
          <Check
            label="Band curves"
            checked={ui.showBandCurves}
            onChange={(on) => {
              ui.showBandCurves = on
              refresh()
            }}
          />
          <Choice
            value={range}
            options={DB_RANGES.map((db): [number, string] => [db, `±${db}`])}
            onChange={(v) => {
              ui.range = v
              refresh()
            }}
          />
        </div>

        <div className="eq-graph">
          <div className="eq-plot-row">
            <div className="eq-db">
              {dbGrid(range).map((db) => (
                <span key={db} className={db === 0 ? 'zero' : ''} style={{ top: `${dbFraction(db, range) * 100}%` }}>
                  {db > 0 ? `+${db}` : `${db}`}
                </span>
              ))}
            </div>
            <div
              ref={plotRef}
              className={`eq-plot${drag ? ' dragging' : ''}`}
              onContextMenu={(e) => e.preventDefault()}
              onPointerLeave={() => {
                if (ui.hover) {
                  ui.hover = null
                  refresh()
                }
              }}
              onPointerCancel={endDrag}
            >
              <LiveCanvas draw={draw} onPointerDown={onDown} onPointerMove={onMove} onPointerUp={onUp}>
                {nodes}
                {readout && <span className="eq-readout">{readout}</span>}
                {hint && (
                  <div className="eq-hint">
                    <span>{hint}</span>
                  </div>
                )}
              </LiveCanvas>
            </div>
          </div>
          <div className="eq-freq">
            {FREQ_LABELS.map(([hz, text]) => (
              <span key={hz} style={{ left: `${freqFraction(hz) * 100}%` }}>
                {text}
              </span>
            ))}
          </div>
        </div>

        <div className="eq-strip">
          {model.listed.map((i) => (
            <StripCell
              key={i}
              kind={kind}
              index={i}
              band={model.bands[i]}
              selected={selected === i}
              soloed={solo === i}
              onSelect={() => select(i)}
              onToggle={() => toggleBand(i)}
            />
          ))}
          {info.zx && (
            <span className="eq-strip-count">
              {model.listed.length === 0
                ? 'No bands yet — double-click the graph to add one'
                : model.listed.length === info.slots
                  ? `${info.slots} / ${info.slots} bands — limit reached`
                  : `${model.listed.length} / ${info.slots} bands`}
            </span>
          )}
        </div>

        <Row>
          {selected !== null && band ? (
            <>
              <BandCard
                editor={editor}
                kind={kind}
                index={selected}
                band={band}
                soloed={solo === selected}
                edit={(change, live) => editBand(selected, change, live)}
                onToggle={() => toggleBand(selected)}
                onSolo={() => toggleSolo(selected)}
                onRemove={() => removeBand(selected)}
              />
              <DynamicsCard
                editor={editor}
                kind={kind}
                index={selected}
                band={band}
                edit={(change, live) => editBand(selected, change, live)}
              />
            </>
          ) : (
            <Card title="Band" grow={5} style={{ minWidth: 'min(380px, 100%)' }}>
              <span className="eq-empty">Select a band, or double-click the graph to add one.</span>
            </Card>
          )}
          <OutputCard editor={editor} kind={kind} />
        </Row>
      </EditorShell>
    </div>
  )
}

/** A band's cell in the strip: number, shape, badges, its on lamp, and its
 *  frequency and gain. Click selects, double-click switches it. */
function StripCell(props: {
  kind: Kind
  index: number
  band: Band
  selected: boolean
  soloed: boolean
  onSelect: () => void
  onToggle: () => void
}) {
  const { band, index } = props
  const last = useRef(0)
  const color = bandColor(index)
  const detail = hasGain(band.shape)
    ? `${formatDb(band.gainDb)} dB`
    : isCut(band.shape) && props.kind === 'zx'
      ? `${SHAPE_SHORT[band.shape]} ${band.slope.toFixed(0)}`
      : SHAPE_SHORT[band.shape]
  const badges: [string, string][] = []
  if (dynamicsLive(band)) badges.push(['D', 'var(--warning)'])
  if (band.placement !== 0) badges.push([PLACEMENT_SHORT[band.placement], 'var(--text-secondary)'])
  if (props.soloed) badges.push(['S', 'var(--state-solo)'])
  return (
    <div
      className={`eq-cell${props.selected ? ' selected' : ''}${band.active ? '' : ' off'}`}
      role="button"
      tabIndex={0}
      onPointerDown={(e) => {
        if (e.button !== 0) return
        const now = performance.now()
        if (now - last.current < DOUBLE_MS) {
          last.current = 0
          props.onToggle()
        } else {
          last.current = now
          props.onSelect()
        }
      }}
    >
      <div className="eq-cell-top">
        <span className="eq-cell-num" style={{ color }}>
          {index + 1}
        </span>
        <span className="eq-cell-shape">{SHAPE_SHORT[band.shape]}</span>
        {badges.map(([text, tone]) => (
          <span key={text} className="eq-badge" style={{ color: tone }}>
            {text}
          </span>
        ))}
        <span
          className="eq-lamp"
          title={band.active ? 'Switch off' : 'Switch on'}
          style={{ borderColor: color, background: band.active ? color : 'transparent' }}
          onPointerDown={(e) => {
            e.stopPropagation()
            if (e.button === 0) props.onToggle()
          }}
        />
      </div>
      <div className="eq-cell-bottom">
        <span>{formatFreq(band.freq)} Hz</span>
        <span>{detail}</span>
      </div>
    </div>
  )
}

// ── The band editor's cards ─────────────────────────────────────────────

/** A knob over a log-mapped range: the knob turns 0–1, the value is
 *  `min·(max/min)^position`. */
const logPosition = (v: number, min: number, max: number) =>
  clamp(Math.log(clamp(v, min, max) / min) / Math.log(max / min), 0, 1)
const logValue = (p: number, min: number, max: number) => min * Math.pow(max / min, clamp(p, 0, 1))

/** A knob cell: the knob in its own units, its caption, its readout. */
function EqKnob(props: {
  caption: string
  value: number
  min: number
  max: number
  defaultValue: number
  bipolar?: boolean
  format: (units: number) => string
  onChange: (units: number) => void
}) {
  return (
    <div className="pe-knob">
      <Knob
        value={props.value}
        min={props.min}
        max={props.max}
        defaultValue={props.defaultValue}
        bipolar={props.bipolar}
        size={KNOB}
        label={`${props.caption} (double-click: default)`}
        caption={props.caption}
        format={props.format}
        onChange={props.onChange}
      />
    </div>
  )
}

/** A knob that does nothing for this band (a cut's gain, an EQ-ZX cut's Q,
 *  dynamics switched off), greyed with the reason as its readout — as the
 *  native editor keeps the row's layout steady. */
function OffKnob(props: { caption: string; why: string }) {
  const c = KNOB / 2
  const r = KNOB / 2 - 2.5
  const at = (f: number): [number, number] => {
    const angle = (-135 + 270 * f) * (Math.PI / 180)
    return [c + r * Math.sin(angle), c - r * Math.cos(angle)]
  }
  const [x0, y0] = at(0)
  const [x1, y1] = at(1)
  return (
    <div className="pe-knob eq-off-knob" aria-disabled>
      <div className="knob">
        <svg width={KNOB} height={KNOB} aria-hidden>
          <path className="knob-track" d={`M ${x0} ${y0} A ${r} ${r} 0 1 1 ${x1} ${y1}`} />
          <circle className="knob-body" cx={c} cy={c} r={r * 0.72} />
        </svg>
        <span className="knob-caption">{props.caption}</span>
        <span className="knob-text">{props.why}</span>
      </div>
    </div>
  )
}

type EditFn = (change: (b: Band) => void, live: boolean) => void

function BandCard(props: {
  editor: Editor
  kind: Kind
  index: number
  band: Band
  soloed: boolean
  edit: EditFn
  onToggle: () => void
  onSolo: () => void
  onRemove: () => void
}) {
  const { editor, kind, index, band, edit } = props
  const info = KINDS[kind]
  const defaults = defaultBand(editor, kind, index)
  const slope = SLOPES.find((s) => Math.abs(s - band.slope) < 0.5) ?? -1
  return (
    <Card title="Band" grow={5} style={{ minWidth: info.zx ? 'min(560px, 100%)' : 'min(380px, 100%)' }}>
      <div className="eq-band-title">
        <span className="eq-band-name" style={{ color: bandColor(index) }}>
          Band {index + 1}
        </span>
        <Check label="On" checked={band.active} onChange={props.onToggle} />
        <Toggle className="eq-solo" on={props.soloed} onClick={props.onSolo} title="Hear this band alone">
          Solo
        </Toggle>
        {info.zx && (
          <button type="button" className="button ghost eq-remove" onClick={props.onRemove}>
            Remove
          </button>
        )}
      </div>
      <div className="eq-choices">
        <Choice
          value={band.shape}
          options={info.shapes.map((s): [Shape, string] => [s, SHAPE_SHORT[s]])}
          onChange={(s) =>
            edit((b) => {
              b.shape = s
            }, false)
          }
        />
        {info.zx && (
          <Choice
            value={band.placement}
            options={([0, 1, 2] as const).map((p): [Placement, string] => [p, PLACEMENT_SHORT[p]])}
            onChange={(p) =>
              edit((b) => {
                b.placement = p
              }, false)
            }
          />
        )}
        {info.zx && isCut(band.shape) && (
          <div className="eq-slope">
            <Choice
              value={slope}
              options={SLOPES.map((s): [number, string] => [s, `${s}`])}
              onChange={(s) =>
                edit((b) => {
                  b.slope = s
                }, false)
              }
            />
            <span className="eq-caption">dB/oct</span>
          </div>
        )}
      </div>
      <div className="pe-knobs">
        <EqKnob
          caption="Freq"
          value={freqFraction(band.freq)}
          min={0}
          max={1}
          defaultValue={freqFraction(defaults.freq)}
          format={(u) => `${formatFreq(freqAtFraction(u))} Hz`}
          onChange={(u) =>
            edit((b) => {
              b.freq = freqAtFraction(u)
            }, true)
          }
        />
        {hasGain(band.shape) ? (
          <EqKnob
            caption="Gain"
            value={band.gainDb}
            min={GAIN_MIN_DB}
            max={GAIN_MAX_DB}
            defaultValue={0}
            bipolar
            format={(v) => `${formatDb(Math.round(v * 10) / 10)} dB`}
            onChange={(v) =>
              edit((b) => {
                b.gainDb = Math.round(v * 10) / 10
              }, true)
            }
          />
        ) : (
          <OffKnob caption="Gain" why="no gain" />
        )}
        {usesQ(kind, band.shape) ? (
          <EqKnob
            caption="Q"
            value={logPosition(band.q, Q_MIN, Q_MAX)}
            min={0}
            max={1}
            defaultValue={logPosition(Math.max(Q_MIN, defaults.q), Q_MIN, Q_MAX)}
            format={(u) => (Math.round(logValue(u, Q_MIN, Q_MAX) * 100) / 100).toFixed(2)}
            onChange={(u) =>
              edit((b) => {
                b.q = Math.round(logValue(u, Q_MIN, Q_MAX) * 100) / 100
              }, true)
            }
          />
        ) : (
          <OffKnob caption="Q" why="by slope" />
        )}
      </div>
    </Card>
  )
}

function DynamicsCard(props: { editor: Editor; kind: Kind; index: number; band: Band; edit: EditFn }) {
  const { editor, kind, index, band, edit } = props
  const defaults = defaultBand(editor, kind, index)
  const can = hasGain(band.shape)
  const on = dynamicsLive(band)
  const why = can ? 'off' : 'no gain'
  return (
    <Card title="Dynamics" grow={3} style={{ minWidth: 'min(300px, 100%)' }}>
      <div className="eq-dyn-head">
        <label className={`pe-check${can ? '' : ' eq-disabled'}`}>
          <input
            type="checkbox"
            checked={on}
            disabled={!can}
            onChange={() =>
              edit((b) => {
                b.dynamic = !b.dynamic
              }, false)
            }
          />
          <span>{can ? 'Dynamic' : 'Dynamic (needs a bell or shelf)'}</span>
        </label>
        {KINDS[kind].zx && (
          <Choice
            value={band.dynBelow ? 1 : 0}
            options={[
              [0, 'Above'],
              [1, 'Below'],
            ]}
            onChange={(v) =>
              edit((b) => {
                b.dynBelow = v === 1
              }, false)
            }
          />
        )}
      </div>
      {on ? (
        <div className="pe-knobs">
          <EqKnob
            caption="Thresh"
            value={band.thresholdDb}
            min={THRESHOLD_MIN_DB}
            max={THRESHOLD_MAX_DB}
            defaultValue={defaults.thresholdDb}
            format={(v) => `${formatDb(Math.round(v * 2) / 2)} dB`}
            onChange={(v) =>
              edit((b) => {
                b.thresholdDb = Math.round(v * 2) / 2
              }, true)
            }
          />
          <EqKnob
            caption="Range"
            value={band.rangeDb}
            min={RANGE_MIN_DB}
            max={RANGE_MAX_DB}
            defaultValue={0}
            bipolar
            format={(v) => `${formatDb(Math.round(v * 10) / 10)} dB`}
            onChange={(v) =>
              edit((b) => {
                b.rangeDb = Math.round(v * 10) / 10
              }, true)
            }
          />
          <EqKnob
            caption="Attack"
            value={logPosition(band.attackMs, ATTACK_MIN_MS, ATTACK_MAX_MS)}
            min={0}
            max={1}
            defaultValue={logPosition(defaults.attackMs, ATTACK_MIN_MS, ATTACK_MAX_MS)}
            format={(u) => formatMs(logValue(u, ATTACK_MIN_MS, ATTACK_MAX_MS))}
            onChange={(u) =>
              edit((b) => {
                b.attackMs = logValue(u, ATTACK_MIN_MS, ATTACK_MAX_MS)
              }, true)
            }
          />
          <EqKnob
            caption="Release"
            value={logPosition(band.releaseMs, RELEASE_MIN_MS, RELEASE_MAX_MS)}
            min={0}
            max={1}
            defaultValue={logPosition(defaults.releaseMs, RELEASE_MIN_MS, RELEASE_MAX_MS)}
            format={(u) => formatMs(logValue(u, RELEASE_MIN_MS, RELEASE_MAX_MS))}
            onChange={(u) =>
              edit((b) => {
                b.releaseMs = logValue(u, RELEASE_MIN_MS, RELEASE_MAX_MS)
              }, true)
            }
          />
        </div>
      ) : (
        <div className="pe-knobs">
          <OffKnob caption="Thresh" why={why} />
          <OffKnob caption="Range" why={why} />
          <OffKnob caption="Attack" why={why} />
          <OffKnob caption="Release" why={why} />
        </div>
      )}
    </Card>
  )
}

function OutputCard(props: { editor: Editor; kind: Kind }) {
  const { editor, kind } = props
  return (
    <Card title="Output" grow={1} style={{ minWidth: 'min(150px, 100%)' }}>
      <div className="pe-knobs">
        <EqKnob
          caption="Output"
          value={editor.value('outputDb')}
          min={OUTPUT_MIN_DB}
          max={OUTPUT_MAX_DB}
          defaultValue={0}
          bipolar
          format={(v) => `${formatDb(Math.round(v * 10) / 10)} dB`}
          onChange={(v) => editor.set('outputDb', clamp(Math.round(v * 10) / 10, OUTPUT_MIN_DB, OUTPUT_MAX_DB))}
        />
        {kind === 'z8' && (
          <EqKnob
            caption="Mix"
            value={editor.value('mix')}
            min={0}
            max={100}
            defaultValue={100}
            format={(v) => `${Math.round(v)} %`}
            onChange={(v) => editor.set('mix', clamp(Math.round(v), 0, 100))}
          />
        )}
      </div>
    </Card>
  )
}

export const editors: Record<string, EditorComponent> = {
  equz8: ({ editor }) => <EqEditor editor={editor} kind="z8" />,
  equzx: ({ editor }) => <EqEditor editor={editor} kind="zx" />,
}
