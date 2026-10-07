// Editors for the band built-ins — the Compressor and the Imager — ported
// from the native band family (components/band_model.rs, band_panel.rs,
// imager_panel.rs). Both split the spectrum at three crossovers dragged on a
// frequency display, with a card per band below:
//
// * the Compressor runs single-band (a transfer curve whose threshold is
//   dragged sideways) or four-band (each band's live reduction hangs from
//   the top of the display);
// * the Imager widens or narrows each band — dragged up and down on the
//   display or on its strip's fader — beside a vectorscope and correlation
//   meters.
//
// The stereo-image painters (vectorscope, goniometer, correlation bars) are
// ported here from plugin_live.rs: only the Imager draws them.

import { useEffect, useRef, useState } from 'react'
import type { PointerEvent, ReactNode } from 'react'
import type { Editor, EditorComponent } from './kit.tsx'
import { Card, Choice, DisplayTag, EditorShell, KitKnob, KNOB, LiveCanvas, ParamCheck, ParamChoice } from './kit.tsx'
import type { KnobSpec, Taper, Unit } from './knobspec.ts'
import type { Live } from './live.ts'
import { POLAR_BINS } from './live.ts'
import type { Ctx } from './paint.ts'
import {
  alpha,
  area,
  colors,
  dashed,
  DENSE_CAPTION,
  freqAtFraction,
  freqFraction,
  label,
  line,
  paintLevelBar,
  paintOperatingPoint,
  paintReductionBar,
  paintSpectrum,
  rect,
  transferPlot,
  UI_XS,
} from './paint.ts'
import './band.css'

// ── The model (band_model.rs) ───────────────────────────────────────────

type Kind = 'comp' | 'imager'

const BANDS = 4
const BAND_NAMES = ['Low', 'Low Mid', 'High Mid', 'High']
/** The closest two crossovers may sit, as a frequency ratio. */
const MIN_CROSSOVER_RATIO = 1.25
const CROSSOVER_IDS = ['crossover1Hz', 'crossover2Hz', 'crossover3Hz']
/** compresser::SIDECHAIN_OFF_HZ */
const SIDECHAIN_OFF_HZ = 20
/** imager::DEFAULT_WIDTH / MAX_WIDTH */
const DEFAULT_WIDTH = 100
const MAX_WIDTH = 200
/** A soloed band rides on these: listening, not sound. */
const KEEP = ['soloBand']

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v))
const bandId = (band: number, field: string) => `band${band + 1}${field}`
const widthId = (band: number) => `width${band + 1}`
const stereoizeId = (band: number) => `stereoize${band + 1}`

/** What both editors read off the params. */
interface Split {
  /** Whether the band split is in play: Multi mode for the Compressor,
   *  Multiband on for the Imager. */
  multiband: boolean
  /** The crossovers as the plug-in stores them. */
  crossovers: number[]
  /** The band edges, low to high, 20 Hz to 20 kHz: the crossovers sorted,
   *  as the DSP sorts them. */
  edges: number[]
  /** The soloed band, if any. */
  solo: number | null
}

function splitOf(editor: Editor, kind: Kind): Split {
  const crossovers = CROSSOVER_IDS.map((id) => editor.value(id))
  const sorted = [...crossovers].sort((a, b) => a - b)
  const solo = Math.round(editor.value('soloBand'))
  return {
    multiband: kind === 'comp' ? editor.value('mode') >= 0.5 : editor.flag('multiband'),
    crossovers,
    edges: [20, ...sorted, 20000],
    solo: solo >= 0 ? solo : null,
  }
}

/** How far crossover `index` may move: past neither neighbour, kept
 *  MIN_CROSSOVER_RATIO clear of each. */
function crossoverBounds(crossovers: number[], index: number): [number, number] {
  const low = index === 0 ? 20 : crossovers[index - 1] * MIN_CROSSOVER_RATIO
  const high = index + 1 === CROSSOVER_IDS.length ? 20000 : crossovers[index + 1] / MIN_CROSSOVER_RATIO
  return [low, Math.max(high, low)]
}

/** A width as Ozone-style imagers read it: −100 folds to mono, 0 leaves the
 *  band as it came, +100 doubles its side. */
function widthReadout(width: number): string {
  const relative = Math.round(width - DEFAULT_WIDTH)
  if (Math.abs(relative) < 0.5) return '0'
  return relative > 0 ? `+${relative}` : `${relative}`
}

/** A frequency for a label: `120`, `1.20k`, `12.0k`. */
function shortHz(hz: number): string {
  if (hz >= 10000) return `${(hz / 1000).toFixed(1)}k`
  if (hz >= 1000) return `${(hz / 1000).toFixed(2)}k`
  return hz.toFixed(0)
}

/** compresser::curve_reduction_db: the gain computer the DSP runs. */
function curveReductionDb(level: number, threshold: number, ratio: number, knee: number): number {
  const slope = 1 - 1 / Math.max(1, ratio)
  const over = level - threshold
  const half = 0.5 * Math.max(0, knee)
  if (over <= -half) return 0
  if (over >= half) return over * slope
  const t = over + half
  return (slope * t * t) / (4 * half)
}

/** A knob's mapping (band_model::knob): the range is the descriptor's, a
 *  band's knobs read like the single band's. */
function knobOf(editor: Editor, kind: Kind, id: string): KnobSpec | null {
  const range = editor.effect.params.find((p) => p.id === id)
  if (!range) return null
  const field = id.startsWith('band') ? id.slice(5) : id
  let found: [string, Taper, Unit] | null = null
  if (kind === 'comp') {
    const table: Record<string, [string, Taper, Unit]> = {
      thresholdDb: ['Thresh', 'linear', 'db'],
      ThresholdDb: ['Thresh', 'linear', 'db'],
      ratio: ['Ratio', 'log', 'ratio'],
      Ratio: ['Ratio', 'log', 'ratio'],
      attackMs: ['Attack', 'log', 'ms'],
      AttackMs: ['Attack', 'log', 'ms'],
      releaseMs: ['Release', 'log', 'ms'],
      ReleaseMs: ['Release', 'log', 'ms'],
      makeupDb: ['Makeup', 'linear', 'db'],
      MakeupDb: ['Makeup', 'linear', 'db'],
      sidechainHpfHz: ['SC HPF', 'square', 'cutHz'],
      kneeDb: ['Knee', 'linear', 'db'],
      mix: ['Mix', 'linear', 'percent'],
    }
    found = table[field] ?? null
  } else if (id.startsWith('width')) {
    found = ['Width', 'linear', 'percent']
  } else if (id.startsWith('stereoize') && id !== 'stereoizeMode') {
    found = ['Stereoize', 'linear', 'percent']
  }
  if (!found && field === 'outputDb') found = ['Output', 'linear', 'db']
  if (!found) return null
  const [labelText, taper, unit] = found
  const knob: KnobSpec = { id, label: labelText, min: range.min, max: range.max, taper, unit }
  if (unit === 'cutHz') knob.cutOff = SIDECHAIN_OFF_HZ
  if (['makeupDb', 'MakeupDb', 'outputDb'].includes(field)) {
    knob.bipolar = true
    knob.centre = 0
  } else if (id.startsWith('width')) {
    knob.bipolar = true
    knob.centre = DEFAULT_WIDTH
  }
  return knob
}

function BandKnob(props: { editor: Editor; kind: Kind; id: string; size?: number }) {
  const spec = knobOf(props.editor, props.kind, props.id)
  return spec ? <KitKnob editor={props.editor} spec={spec} size={props.size} /> : null
}

// ── Theme (beyond paint.ts's) ───────────────────────────────────────────

interface More {
  warning: string
  disabled: string
  raised: string
  canvas: string
  strong: string
}
let more: More | null = null
function moreColors(): More {
  if (more) return more
  const s = getComputedStyle(document.documentElement)
  const v = (name: string) => s.getPropertyValue(name).trim()
  more = {
    warning: v('--warning'),
    disabled: v('--text-disabled'),
    raised: v('--surface-raised'),
    canvas: v('--surface-canvas'),
    strong: v('--border-strong'),
  }
  return more
}

/** A rounded box with a hairline edge, as a GPUI quad with a border. */
function framed(ctx: Ctx, x: number, y: number, w: number, h: number, fill: string, edge: string, radius = 3) {
  ctx.beginPath()
  ctx.roundRect(x + 0.5, y + 0.5, Math.max(0, w - 1), Math.max(0, h - 1), radius)
  ctx.fillStyle = fill
  ctx.fill()
  ctx.lineWidth = 1
  ctx.strokeStyle = edge
  ctx.stroke()
}

// ── Geometry ────────────────────────────────────────────────────────────

type Plot = [number, number, number, number]

/** The transfer display's input range, to 0 dBFS. */
const TRANSFER_RANGE = -60
/** How near a press must land to a crossover to take it. */
const CROSSOVER_HIT = 8
/** Drag travel under Shift. */
const FINE = 0.2
const FREQ_TICKS: [number, string][] = [
  [20, '20'],
  [50, '50'],
  [100, '100'],
  [200, '200'],
  [500, '500'],
  [1000, '1k'],
  [2000, '2k'],
  [5000, '5k'],
  [10000, '10k'],
  [20000, '20k'],
]

/** The band display's plot: room for the tags above and the frequency
 *  labels below. */
const bandPlot = (w: number, h: number): Plot => [0, 26, w, Math.max(1, h - 26 - 18)]
const xOf = (hz: number, plot: Plot) => plot[0] + freqFraction(hz) * plot[2]

/** The crossover a press at `x` takes, if any. */
function crossoverAt(split: Split, x: number, plot: Plot): number | null {
  if (!split.multiband) return null
  let best: number | null = null
  let bestDistance = Infinity
  split.crossovers.forEach((hz, i) => {
    const distance = Math.abs(xOf(hz, plot) - x)
    if (distance <= CROSSOVER_HIT && distance < bestDistance) {
      best = i
      bestDistance = distance
    }
  })
  return best
}

/** The band under `x`; with the split off, one band works the whole signal. */
function bandAt(split: Split, x: number, plot: Plot): number | null {
  if (!split.multiband) return 0
  for (let b = 0; b < BANDS; b++) {
    if (x >= xOf(split.edges[b], plot) && x < xOf(split.edges[b + 1], plot)) return b
  }
  return null
}

/** A gesture on a display. */
type Drag =
  | { kind: 'crossover'; index: number }
  /** A band's width dragged on the display or its fader, from where the
   *  press began and every band's width then (Link Bands moves them all). */
  | { kind: 'width' | 'fader'; band: number; originY: number; start: number[] }
  | { kind: 'threshold' }

const dragBand = (drag: Drag | null) => (drag && (drag.kind === 'width' || drag.kind === 'fader') ? drag.band : null)

// ── Static painting ─────────────────────────────────────────────────────

/** The single-band curve: the knee zone shaded, the threshold's handle, and
 *  the gain computer the DSP runs. */
function paintCompTransfer(ctx: Ctx, w: number, h: number, threshold: number, ratio: number, knee: number, bypassed: boolean) {
  const c = colors()
  const m = moreColors()
  const [x0, y0, pw, ph] = transferPlot(w, h)
  const range = TRANSFER_RANGE
  const xAt = (db: number) => x0 + ((db - range) / -range) * pw
  const yAt = (db: number) => y0 + ph - ((clamp(db, range, 0) - range) / -range) * ph
  for (const db of [-48, -36, -24, -12, 0]) {
    rect(ctx, xAt(db), y0, 1, ph, alpha(c.text, 0.06))
    rect(ctx, x0, yAt(db), pw, 1, alpha(c.text, 0.06))
    if (db < 0) {
      label(ctx, db.toFixed(0), DENSE_CAPTION, c.textFaint, xAt(db), y0 + ph + 4, 'center')
      label(ctx, db.toFixed(0), DENSE_CAPTION, c.textFaint, x0 - 6, yAt(db) - 6, 'right')
    }
  }
  const kneeLeft = xAt(Math.max(range, threshold - knee * 0.5))
  const kneeRight = xAt(Math.min(0, threshold + knee * 0.5))
  rect(ctx, kneeLeft, y0, Math.max(0, kneeRight - kneeLeft), ph, alpha(c.accent, 0.06))
  dashed(ctx, [x0, y0 + ph], [x0 + pw, y0], 1, alpha(c.text, 0.22))
  const points = Array.from({ length: 161 }, (_, i): [number, number] => {
    const input = range - (range * i) / 160
    const output = bypassed ? input : input - curveReductionDb(input, threshold, ratio, knee)
    return [xAt(input), yAt(output)]
  })
  const a = bypassed ? 0.35 : 1
  area(ctx, points, y0 + ph, alpha(c.accent, 0.08 * a))
  line(ctx, points, 2, alpha(c.accent, a))

  // The threshold's handle: a line and its tag.
  const x = xAt(threshold)
  rect(ctx, x - 0.5, y0, 1, ph, alpha(m.warning, 0.8))
  rect(ctx, x - 26, y0 - 18, 52, 15, m.warning, 3)
  label(ctx, threshold.toFixed(1), DENSE_CAPTION, m.canvas, x, y0 - 17, 'center')
}

interface ImagerBands {
  widths: number[]
  stereoize: number[]
  /** "I" or "II". */
  mode: string
}

/** The band display's frame: the frequency grid and labels, the crossover
 *  handles and — for the Imager — each band's width as a box around the
 *  centre line. */
function paintBandDisplay(
  ctx: Ctx,
  w: number,
  h: number,
  split: Split,
  imager: ImagerBands | null,
  drag: Drag | null,
  bypassed: boolean,
) {
  const c = colors()
  const m = moreColors()
  const plot = bandPlot(w, h)
  const [x0, y0, pw, ph] = plot
  for (const [hz, text] of FREQ_TICKS) {
    const x = xOf(hz, plot)
    const major = hz === 100 || hz === 1000 || hz === 10000
    rect(ctx, x, y0, 1, ph, alpha(c.text, major ? 0.09 : 0.045))
    const align = hz === 20 ? 'left' : hz === 20000 ? 'right' : 'center'
    label(ctx, text, DENSE_CAPTION, c.textFaint, x, y0 + ph + 3, align)
  }
  const { edges, solo } = split
  if (imager) {
    const centre = y0 + ph * 0.5
    const half = Math.max(1, ph * 0.5 - 8)
    dashed(ctx, [x0, centre], [x0 + pw, centre], 1, alpha(c.text, 0.12))
    for (const unity of [centre - half * 0.5, centre + half * 0.5]) {
      dashed(ctx, [x0, unity], [x0 + pw, unity], 1, alpha(c.text, 0.08))
    }
    const shown = split.multiband ? BANDS : 1
    const active = dragBand(drag)
    for (let band = 0; band < shown; band++) {
      const [left, right] = split.multiband
        ? [xOf(edges[band], plot) + 2, xOf(edges[band + 1], plot) - 2]
        : [x0 + 2, x0 + pw - 2]
      if (right <= left) continue
      const width = imager.widths[band]
      const reach = Math.max(0.75, (width / MAX_WIDTH) * half)
      const muted = solo !== null && solo !== band
      const tone = muted ? m.disabled : c.accent
      const [fillA, edgeA] = active === band ? [0.26, 0.9] : [0.16, 0.55]
      const dim = bypassed ? 0.35 : 1
      framed(ctx, left, centre - reach, right - left, 2 * reach, alpha(tone, fillA * dim), alpha(tone, edgeA * dim))
      if (right - left > 54) {
        const text = split.multiband
          ? `${BAND_NAMES[band]} ${widthReadout(width)}`
          : `Width ${widthReadout(width)}`
        label(ctx, text, DENSE_CAPTION, c.textSecondary, (left + right) * 0.5, y0 + 4, 'center')
        if (imager.stereoize[band] >= 0.5) {
          const amount = `Stereoize ${imager.mode} ${imager.stereoize[band].toFixed(0)}%`
          label(ctx, amount, DENSE_CAPTION, c.accentHover, (left + right) * 0.5, y0 + 17, 'center')
        }
      }
    }
  }
  // With the split off there are no band edges to show.
  if (!split.multiband) return
  split.crossovers.forEach((hz, i) => {
    const x = xOf(hz, plot)
    const active = drag?.kind === 'crossover' && drag.index === i
    const tone = active ? m.warning : alpha(c.textSecondary, 0.8)
    rect(ctx, x - 0.5, y0, 1, ph, tone)
    framed(ctx, x - 22, y0 + ph - 17, 44, 15, m.raised, tone)
    label(ctx, shortHz(hz), DENSE_CAPTION, c.text, x, y0 + ph - 16, 'center')
  })
}

/** Each band's reduction hanging from the top of its stretch of the
 *  display, over 24 dB, with its name and reading. */
function paintBandReduction(
  ctx: Ctx,
  plot: Plot,
  split: Split,
  bandBypass: boolean[],
  reduction: number[] | null,
  bypassed: boolean,
) {
  const c = colors()
  const m = moreColors()
  const [, y0, , ph] = plot
  for (let band = 0; band < BANDS; band++) {
    const left = xOf(split.edges[band], plot)
    const right = xOf(split.edges[band + 1], plot)
    if (right - left < 2) continue
    const off = bypassed || bandBypass[band]
    const muted = off || (split.solo !== null && split.solo !== band)
    const db = off ? 0 : (reduction?.[band] ?? 0)
    const depth = clamp(db / 24, 0, 1) * ph
    const tone = muted ? m.disabled : c.accent
    if (depth > 0.5) {
      rect(ctx, left + 1, y0, right - left - 2, depth, alpha(tone, 0.2))
      rect(ctx, left + 1, y0 + depth - 1, right - left - 2, 1.5, tone)
    }
    if (right - left > 54) {
      const text = bandBypass[band] ? `${BAND_NAMES[band]} bypassed` : `${BAND_NAMES[band]} −${db.toFixed(1)} dB`
      label(ctx, text, DENSE_CAPTION, c.textSecondary, (left + right) * 0.5, y0 + 4, 'center')
    }
  }
}

/** A width fader: its track, the unchanged mark at the middle, the travel
 *  from there to the setting, and the cap. */
function paintFader(ctx: Ctx, w: number, h: number, width: number, active: boolean, live: boolean) {
  const c = colors()
  const m = moreColors()
  const centreX = w * 0.5
  const yOf = (value: number) => (1 - value / MAX_WIDTH) * h
  rect(ctx, centreX - 3, 0, 6, h, c.meterBg, 3)
  for (const mark of [0, 50, 150, MAX_WIDTH]) rect(ctx, 3, yOf(mark) - 0.5, w - 6, 1, alpha(c.text, 0.1))
  const unchanged = yOf(DEFAULT_WIDTH)
  rect(ctx, 0, unchanged - 0.5, w, 1, alpha(c.text, 0.35))
  const at = yOf(width)
  const tone = !live ? m.disabled : width < DEFAULT_WIDTH ? c.textSecondary : c.accent
  const [top, bottom] = at < unchanged ? [at, unchanged] : [unchanged, at]
  rect(ctx, centreX - 3, top, 6, bottom - top, tone)
  framed(ctx, 2, at - 5, w - 4, 10, active ? c.accentHover : m.raised, m.strong)
}

// ── Stereo-image painters (plugin_live.rs) ──────────────────────────────

/** Which picture of the stereo image a vectorscope draws. */
type ScopeMode = 'polarSample' | 'polarLevel' | 'lissajous'
const SCOPE_MODES: [ScopeMode, string, string][] = [
  // [mode, its name on a switch, its full name]
  ['polarSample', 'Sample', 'Polar Sample'],
  ['polarLevel', 'Level', 'Polar Level'],
  ['lissajous', 'Lissajous', 'Lissajous'],
]
/** Output samples a scope frame carries, as left/right pairs. */
const IMAGE_SCOPE_POINTS = 128

/** The scope frames to draw: none once the insert stops reporting, so a
 *  stopped insert never shows a frozen picture as if it were live. */
const scopeFrames = (live: Live) => (live.frame ? live.images : [])

function paintVectorscope(ctx: Ctx, w: number, h: number, live: Live, mode: ScopeMode) {
  if (mode === 'lissajous') paintScope(ctx, w, h, live)
  else paintPolar(ctx, w, h, live, mode === 'polarLevel')
}

/** A left/right pair as a polar point folded into the upper half plane. */
function polarOf(left: number, right: number): [number, number] {
  let mid = (left + right) * Math.SQRT1_2
  let side = (right - left) * Math.SQRT1_2
  // A point below the axis is the same direction reached with the polarity
  // flipped: fold it up, as a polar scope does.
  if (mid < 0) {
    mid = -mid
    side = -side
  }
  return [Math.atan2(side, mid), Math.sqrt(mid * mid + side * side)]
}

/** The half-circle polar scope: mono straight up, left and right on the
 *  diagonals, out-of-phase energy down at the baseline either side. */
function paintPolar(ctx: Ctx, w: number, h: number, live: Live, levels: boolean) {
  const c = colors()
  const centre: [number, number] = [w * 0.5, h - 18]
  const radius = Math.max(8, Math.min(w * 0.5 - 16, h - 40))
  const at = (angle: number, r: number): [number, number] => [
    centre[0] + r * Math.sin(angle),
    centre[1] - r * Math.cos(angle),
  ]
  for (const r of [radius, radius * 0.5]) {
    const arc = Array.from({ length: 49 }, (_, i) => at(-Math.PI / 2 + (Math.PI * i) / 48, r))
    line(ctx, arc, 1, alpha(c.text, 0.08))
  }
  line(ctx, [at(-Math.PI / 2, radius), at(Math.PI / 2, radius)], 1, alpha(c.text, 0.12))
  for (const angle of [-Math.PI / 4, 0, Math.PI / 4]) line(ctx, [centre, at(angle, radius)], 1, alpha(c.text, 0.12))
  const faint = c.textFaint
  const [mx, my] = at(0, radius + 4)
  label(ctx, 'M', DENSE_CAPTION, faint, mx, my - 12, 'center')
  const [lx, ly] = at(-Math.PI / 4, radius + 6)
  label(ctx, 'L', DENSE_CAPTION, faint, lx - 4, ly - 12, 'center')
  const [rx, ry] = at(Math.PI / 4, radius + 6)
  label(ctx, 'R', DENSE_CAPTION, faint, rx + 4, ry - 12, 'center')
  label(ctx, '+S', DENSE_CAPTION, faint, centre[0] + radius + 4, centre[1] - 6, 'left')
  label(ctx, '−S', DENSE_CAPTION, faint, centre[0] - radius - 4, centre[1] - 6, 'right')
  const frames = scopeFrames(live)
  if (frames.length === 0) return
  label(ctx, `×${live.scopeGain.toFixed(1)}`, DENSE_CAPTION, faint, w - 8, 8, 'right')
  if (levels) {
    live.polar.forEach((level, bin) => {
      const length = Math.min(1, level * live.scopeGain) * radius
      if (length < 0.5) return
      const angle = (bin / (POLAR_BINS - 1) - 0.5) * Math.PI
      line(ctx, [centre, at(angle, length)], 2, alpha(c.accentHover, 0.85))
    })
    return
  }
  frames.forEach((frame, age) => {
    ctx.fillStyle = alpha(c.accentHover, 0.18 + (0.72 * (age + 1)) / frames.length)
    const pairs = Math.min(IMAGE_SCOPE_POINTS, Math.floor(frame.scope.length / 2))
    for (let i = 0; i < pairs; i++) {
      const [angle, r] = polarOf(frame.scope[2 * i], frame.scope[2 * i + 1])
      const [x, y] = at(angle, Math.min(1, r * live.scopeGain) * radius)
      ctx.fillRect(x - 1, y - 1, 2, 2)
    }
  })
}

/** The goniometer: mid up, side across, the last frames fading in, scaled
 *  by the auto-gain. */
function paintScope(ctx: Ctx, w: number, h: number, live: Live) {
  const c = colors()
  const centre: [number, number] = [w * 0.5, h * 0.5 + 6]
  const radius = Math.max(8, Math.min(w, h - 24) * 0.5 - 10)
  for (const r of [radius, radius * 0.5]) {
    const ring = Array.from({ length: 49 }, (_, i): [number, number] => {
      const a = (i / 48) * Math.PI * 2
      return [centre[0] + r * Math.cos(a), centre[1] + r * Math.sin(a)]
    })
    line(ctx, ring, 1, alpha(c.text, 0.07))
  }
  const diagonal = radius * Math.SQRT1_2
  for (const [dx, dy] of [
    [0, radius],
    [radius, 0],
    [diagonal, diagonal],
    [diagonal, -diagonal],
  ]) {
    line(ctx, [[centre[0] - dx, centre[1] - dy], [centre[0] + dx, centre[1] + dy]], 1, alpha(c.text, 0.12))
  }
  const faint = c.textFaint
  label(ctx, 'M', DENSE_CAPTION, faint, centre[0], centre[1] - radius - 13, 'center')
  label(ctx, 'L', DENSE_CAPTION, faint, centre[0] - diagonal - 6, centre[1] - diagonal - 12, 'center')
  label(ctx, 'R', DENSE_CAPTION, faint, centre[0] + diagonal + 6, centre[1] - diagonal - 12, 'center')
  label(ctx, 'S', DENSE_CAPTION, faint, centre[0] + radius + 8, centre[1] - 6, 'center')
  const frames = scopeFrames(live)
  if (frames.length === 0) return
  label(ctx, `×${live.scopeGain.toFixed(1)}`, DENSE_CAPTION, faint, w - 8, 8, 'right')
  frames.forEach((frame, age) => {
    ctx.fillStyle = alpha(c.accentHover, 0.18 + (0.72 * (age + 1)) / frames.length)
    const pairs = Math.min(IMAGE_SCOPE_POINTS, Math.floor(frame.scope.length / 2))
    for (let i = 0; i < pairs; i++) {
      const l = frame.scope[2 * i] * live.scopeGain
      const r = frame.scope[2 * i + 1] * live.scopeGain
      const side = clamp((r - l) * Math.SQRT1_2, -1, 1)
      const mid = clamp((l + r) * Math.SQRT1_2, -1, 1)
      ctx.fillRect(centre[0] + side * radius - 1, centre[1] - mid * radius - 1, 2, 2)
    }
  })
}

/** A correlation bar, −1 to +1, growing from the centre: accent toward
 *  mono, warning toward out of phase. Null reads as no signal. */
function paintCorrelation(ctx: Ctx, w: number, h: number, value: number | null, withScale: boolean) {
  const c = colors()
  const m = moreColors()
  const barH = withScale ? Math.max(4, h - 30) : Math.min(h, 8)
  const barY = withScale ? 14 : (h - barH) * 0.5
  rect(ctx, 0, barY, w, barH, c.meterBg)
  const centre = w * 0.5
  if (value !== null) {
    const v = clamp(value, -1, 1)
    const end = centre + v * w * 0.5
    const [left, right] = v >= 0 ? [centre, end] : [end, centre]
    rect(ctx, left, barY, Math.max(1, right - left), barH, v >= 0 ? c.accent : m.warning)
  }
  rect(ctx, centre - 0.5, barY - 2, 1, barH + 4, c.textSecondary)
  if (!withScale) return
  label(ctx, '−1', DENSE_CAPTION, c.textFaint, 0, barY + barH + 2, 'left')
  label(ctx, '0', DENSE_CAPTION, c.textFaint, centre, barY + barH + 2, 'center')
  label(ctx, '+1', DENSE_CAPTION, c.textFaint, w, barY + barH + 2, 'right')
  const text = value === null ? '—' : `${value >= 0 ? '+' : '-'}${Math.abs(value).toFixed(2)}`
  label(ctx, text, UI_XS, c.textSecondary, w, 0, 'right')
  label(ctx, 'Correlation', DENSE_CAPTION, c.textMuted, 0, 1, 'left')
}

/** A correlation bar standing up, −1 at the foot to +1 at the head, growing
 *  from the middle, in a box `pad` wider each side than the bar (its centre
 *  mark reaches past the bar, as the native one does). */
function paintCorrelationVertical(ctx: Ctx, w: number, h: number, value: number | null, pad: number) {
  const c = colors()
  const m = moreColors()
  const barW = w - 2 * pad
  rect(ctx, pad, 0, barW, h, c.meterBg)
  const centre = h * 0.5
  if (value !== null) {
    const v = clamp(value, -1, 1)
    const end = centre - v * h * 0.5
    const [top, bottom] = v >= 0 ? [end, centre] : [centre, end]
    rect(ctx, pad, top, barW, Math.max(1, bottom - top), v >= 0 ? c.accent : m.warning)
  }
  rect(ctx, 0, centre - 0.5, w, 1, c.textSecondary)
}

// ── Pieces ──────────────────────────────────────────────────────────────

/** A small switch pill: on lights it in `tone` (a CSS colour). */
function Pill(props: { on: boolean; tone: string; onClick: () => void; children: ReactNode }) {
  return (
    <button
      type="button"
      className={`bd-pill${props.on ? ' on' : ''}`}
      style={{ ['--tone' as string]: props.tone }}
      aria-pressed={props.on}
      onClick={props.onClick}
    >
      {props.children}
    </button>
  )
}

/** A framed live readout (meters, correlation) at a fixed height. */
function Well(props: { height: number; draw: (ctx: Ctx, w: number, h: number) => void }) {
  return (
    <div className="bd-well" style={{ height: props.height }}>
      <LiveCanvas className="bd-bare bd-fill" draw={props.draw} />
    </div>
  )
}

/** The input/output bars (and the reduction, for the Compressor) in rows. */
function paintMeterRows(ctx: Ctx, w: number, h: number, live: Live, rows: ('reduction' | 'in' | 'out')[], gap: number, bypassed: boolean) {
  const frame = live.frame
  const rowH = h / rows.length
  rows.forEach((row, i) => {
    const rh = Math.max(16, rowH - gap)
    ctx.save()
    ctx.translate(0, i * rowH)
    if (row === 'reduction') paintReductionBar(ctx, w, rh, 'Reduction', frame ? (bypassed ? 0 : frame.gain_reduction_db) : null)
    else if (row === 'in') paintLevelBar(ctx, w, rh, 'In', frame?.in_peak ?? null, frame?.in_rms ?? null)
    else paintLevelBar(ctx, w, rh, 'Out', frame?.out_peak ?? null, frame?.out_rms ?? null)
    ctx.restore()
  })
}

const bypassNotice = (title: string) => `Bypassed — ${title} passes audio through unchanged`

/** Shared state of a band editor: the gesture in flight, and the solo let
 *  go by Esc and when the editor closes (a closed editor never leaves a
 *  track playing one band). */
function useBandEditor(editor: Editor) {
  const drag = useRef<Drag | null>(null)
  const latest = useRef(editor)
  latest.current = editor
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || e.defaultPrevented) return
      const target = e.target as HTMLElement | null
      if (target && (target.tagName === 'INPUT' || target.tagName === 'SELECT' || target.tagName === 'TEXTAREA')) return
      if (latest.current.value('soloBand') >= 0) latest.current.set('soloBand', -1)
    }
    window.addEventListener('keydown', onKey)
    return () => {
      window.removeEventListener('keydown', onKey)
      if (latest.current.value('soloBand') >= 0) latest.current.set('soloBand', -1)
    }
  }, [])
  return drag
}

/** The band display: crossovers dragged sideways, and for the Imager each
 *  band's width dragged up and down. */
function BandDisplay(props: {
  editor: Editor
  kind: Kind
  drag: { current: Drag | null }
  linkBands: boolean
  legend: string
  notice: string | null
}) {
  const { editor, kind, drag } = props
  const split = splitOf(editor, kind)
  const imager: ImagerBands | null =
    kind === 'imager'
      ? {
          widths: Array.from({ length: BANDS }, (_, b) => editor.value(widthId(b))),
          stereoize: Array.from({ length: BANDS }, (_, b) => editor.value(stereoizeId(b))),
          mode: Math.round(editor.value('stereoizeMode')) >= 1 ? 'II' : 'I',
        }
      : null
  const bandBypass = Array.from({ length: BANDS }, (_, b) => editor.flag(bandId(b, 'Bypass')))
  const bypassed = editor.bypassed
  const live = editor.live

  const plotOf = (e: { currentTarget: HTMLDivElement }) =>
    bandPlot(e.currentTarget.clientWidth, e.currentTarget.clientHeight)

  const down = (x: number, y: number, e: PointerEvent<HTMLDivElement>) => {
    const plot = plotOf(e)
    const nearest = crossoverAt(split, x, plot)
    const band = bandAt(split, x, plot)
    if (nearest !== null) drag.current = { kind: 'crossover', index: nearest }
    else if (imager && band !== null) drag.current = { kind: 'width', band, originY: y, start: imager.widths }
    else drag.current = null
  }
  const move = (x: number, y: number, e: PointerEvent<HTMLDivElement>) => {
    const plot = plotOf(e)
    const gesture = drag.current
    if (!gesture || e.buttons === 0) {
      drag.current = null
      // The pointer tells what a press would take.
      e.currentTarget.style.cursor =
        crossoverAt(split, x, plot) !== null ? 'ew-resize' : imager && bandAt(split, x, plot) !== null ? 'ns-resize' : ''
      return
    }
    if (gesture.kind === 'crossover') {
      const [low, high] = crossoverBounds(split.crossovers, gesture.index)
      const hz = Math.round(clamp(freqAtFraction((x - plot[0]) / plot[2]), low, high))
      editor.set(CROSSOVER_IDS[gesture.index], hz)
    } else if (gesture.kind === 'width') {
      const half = Math.max(1, plot[3] * 0.5 - 8)
      const scale = e.shiftKey ? FINE : 1
      dragWidths(editor, split, props.linkBands, gesture.band, gesture.start, ((gesture.originY - y) / half) * 100 * scale)
    }
  }
  const up = () => {
    drag.current = null
  }
  // A double-click puts back what it lands on.
  const reset = (x: number, _y: number, e: { currentTarget: HTMLDivElement }) => {
    const plot = plotOf(e)
    const nearest = crossoverAt(split, x, plot)
    if (nearest !== null) {
      editor.set(CROSSOVER_IDS[nearest], editor.defaultOf(CROSSOVER_IDS[nearest]))
      return
    }
    const band = bandAt(split, x, plot)
    if (imager && band !== null) resetWidth(editor, split, props.linkBands, band)
  }

  return (
    <LiveCanvas
      className="bd-display"
      draw={(ctx, w, h) => {
        paintBandDisplay(ctx, w, h, split, imager, drag.current, bypassed)
        const plot = bandPlot(w, h)
        paintSpectrum(ctx, live, plot)
        if (kind === 'comp') paintBandReduction(ctx, plot, split, bandBypass, live.frame ? live.bandReduction : null, bypassed)
      }}
      onPointerDown={down}
      onPointerMove={move}
      onPointerUp={up}
      onDoubleClick={reset}
    >
      <DisplayTag>Bands</DisplayTag>
      <DisplayTag right>{props.legend}</DisplayTag>
      {props.notice && <span className="bd-notice">{props.notice}</span>}
    </LiveCanvas>
  )
}

/** The bands a width gesture on `band` moves: all of them with Link Bands
 *  on and the split in play, else that one. */
const movedBands = (split: Split, linkBands: boolean, band: number) =>
  linkBands && split.multiband ? Array.from({ length: BANDS }, (_, b) => b) : [band]

/** Moves the gesture's bands `delta` percent from where they started. */
function dragWidths(editor: Editor, split: Split, linkBands: boolean, band: number, start: number[], delta: number) {
  const next: Record<string, number> = {}
  for (const b of movedBands(split, linkBands, band)) next[widthId(b)] = Math.round(clamp(start[b] + delta, 0, MAX_WIDTH))
  editor.setMany(next)
}

/** Puts the gesture's bands back to unchanged. */
function resetWidth(editor: Editor, split: Split, linkBands: boolean, band: number) {
  const next: Record<string, number> = {}
  for (const b of movedBands(split, linkBands, band)) next[widthId(b)] = DEFAULT_WIDTH
  editor.setMany(next)
}

/** A band card's frame: a soloed card stands out, the rest step back while
 *  one is soloed. */
function bandCardStyle(split: Split, band: number, off = false) {
  const soloed = split.solo === band
  return {
    className: `bd-band${soloed ? ' soloed' : ''}`,
    style: { opacity: off ? 0.38 : split.solo !== null && !soloed ? 0.55 : 1 },
  }
}

const rangeText = (split: Split, band: number) => `${shortHz(split.edges[band])} – ${shortHz(split.edges[band + 1])} Hz`

// ── The Compressor ──────────────────────────────────────────────────────

function Compressor(props: { editor: Editor }) {
  const { editor } = props
  const drag = useBandEditor(editor)
  const split = splitOf(editor, 'comp')
  const multi = split.multiband
  const bypassed = editor.bypassed
  const notice = bypassed ? bypassNotice('Compressor') : null
  const live = editor.live
  const k = (id: string) => <BandKnob key={id} editor={editor} kind="comp" id={id} />

  const threshold = editor.value('thresholdDb')
  const ratio = editor.value('ratio')
  const knee = editor.value('kneeDb')
  const setThresholdAt = (x: number, e: { currentTarget: HTMLDivElement }) => {
    const [x0, , w] = transferPlot(e.currentTarget.clientWidth, e.currentTarget.clientHeight)
    const db = TRANSFER_RANGE - ((x - x0) / w) * TRANSFER_RANGE
    editor.set('thresholdDb', Math.round(clamp(db, TRANSFER_RANGE, 0) * 10) / 10)
  }

  const main = multi ? (
    <BandDisplay editor={editor} kind="comp" drag={drag} linkBands={false} legend="Reduction per band" notice={notice} />
  ) : (
    <LiveCanvas
      className="bd-display bd-transfer"
      draw={(ctx, w, h) => {
        paintCompTransfer(ctx, w, h, threshold, ratio, knee, bypassed)
        if (!bypassed) {
          paintOperatingPoint(ctx, w, h, live, TRANSFER_RANGE, 0, (input) => input - curveReductionDb(input, threshold, ratio, knee))
        }
      }}
      onPointerDown={(x, _y, e) => {
        drag.current = { kind: 'threshold' }
        setThresholdAt(x, e)
      }}
      onPointerMove={(x, _y, e) => {
        if (drag.current?.kind !== 'threshold') return
        if (e.buttons === 0) drag.current = null
        else setThresholdAt(x, e)
      }}
      onPointerUp={() => {
        drag.current = null
      }}
      onDoubleClick={() => editor.set('thresholdDb', editor.defaultOf('thresholdDb'))}
    >
      <DisplayTag>Transfer</DisplayTag>
      <DisplayTag right>{`${threshold.toFixed(1)} dB · ${ratio.toFixed(1)}:1 · knee ${knee.toFixed(1)} dB`}</DisplayTag>
      {notice && <span className="bd-notice">{notice}</span>}
    </LiveCanvas>
  )

  return (
    <EditorShell
      editor={editor}
      title="Compressor"
      subtitle={multi ? 'Four-band compression' : 'Single-band compression'}
      keep={KEEP}
    >
      <div className="bd">
        <div className="bd-mode">
          <span className="bd-caption">Mode</span>
          <ParamChoice
            editor={editor}
            id="mode"
            options={[
              [0, 'Single'],
              [1, 'Multi'],
            ]}
          />
          <span className="bd-hint">
            {multi ? 'Drag a crossover to move a band edge · Esc lets a solo go' : 'Drag across the curve to set the threshold'}
          </span>
        </div>
        <div className="bd-main">
          {main}
          <div className="bd-aside">
            <Well height={124} draw={(ctx, w, h) => paintMeterRows(ctx, w, h, live, ['reduction', 'in', 'out'], 8, bypassed)} />
            <Card title="Output">
              <div className="pe-knobs bd-knobs">{['kneeDb', 'mix', 'outputDb'].map(k)}</div>
            </Card>
          </div>
        </div>
        <div className="bd-bottom">
          {multi ? (
            Array.from({ length: BANDS }, (_, band) => {
              const bypass = editor.flag(bandId(band, 'Bypass'))
              const soloed = split.solo === band
              const frame = bandCardStyle(split, band)
              return (
                <Card
                  key={band}
                  title={BAND_NAMES[band]}
                  aside={<span className="bd-hint">{rangeText(split, band)}</span>}
                  minWidth={212}
                  {...frame}
                >
                  <div className="bd-switches">
                    <Pill on={bypass} tone="var(--text-secondary)" onClick={() => editor.set(bandId(band, 'Bypass'), bypass ? 0 : 1)}>
                      Bypass
                    </Pill>
                    <Pill on={soloed} tone="var(--warning)" onClick={() => editor.set('soloBand', soloed ? -1 : band)}>
                      Solo
                    </Pill>
                  </div>
                  <LiveCanvas
                    className="bd-bare bd-reduction"
                    draw={(ctx, w, h) => {
                      const off = bypassed || bypass
                      const reduction = live.frame ? live.bandReduction : null
                      paintReductionBar(ctx, w, h, 'Reduction', reduction ? (off ? 0 : (reduction[band] ?? 0)) : null)
                    }}
                  />
                  <div className="pe-knobs bd-knobs" style={{ opacity: bypass ? 0.38 : 1 }}>
                    {['ThresholdDb', 'Ratio', 'MakeupDb', 'AttackMs', 'ReleaseMs'].map((field) => k(bandId(band, field)))}
                  </div>
                </Card>
              )
            })
          ) : (
            <Card title="Compression" grow={6}>
              <div className="pe-knobs bd-knobs">
                {['thresholdDb', 'ratio', 'attackMs', 'releaseMs', 'makeupDb', 'sidechainHpfHz'].map(k)}
              </div>
            </Card>
          )}
        </div>
      </div>
    </EditorShell>
  )
}

// ── The Imager ──────────────────────────────────────────────────────────

/** A width fader's travel. */
const FADER_H = 128
const FADER_W = 30
/** The correlation bar's width, and the room its centre mark reaches past
 *  it either side. */
const CORRELATION_W = 6
const CORRELATION_PAD = 2

function Imager(props: { editor: Editor }) {
  const { editor } = props
  const drag = useBandEditor(editor)
  // Editor settings, not params: what the vectorscope draws, and whether a
  // width gesture moves every band.
  const [scope, setScope] = useState<ScopeMode>('polarSample')
  const [linkBands, setLinkBands] = useState(false)
  const split = splitOf(editor, 'imager')
  const multi = split.multiband
  const live = editor.live
  const notice = editor.bypassed ? bypassNotice('Imager') : null
  const stereoizeMode = Math.round(editor.value('stereoizeMode')) >= 1 ? 1 : 0

  /** The overall correlation, while there is something to correlate. */
  const overall = () => {
    const frame = live.frame
    const image = frame ? live.image : null
    return image && frame && frame.out_rms > 1e-4 ? image.correlation : null
  }
  const bandCorrelation = (band: number) => {
    const image = live.frame ? live.image : null
    if (multi) return image && (image.band_level[band] ?? 0) > 1e-4 ? (image.band_correlation[band] ?? null) : null
    return band === 0 ? overall() : null
  }

  /** Every band's width now. */
  const widths = () => Array.from({ length: BANDS }, (_, b) => editor.value(widthId(b)))

  const strips = Array.from({ length: BANDS }, (_, band) => {
    // With the split off, the first strip works the whole signal and the
    // rest stand aside.
    const active = multi || band === 0
    const solo = multi ? split.solo : null
    const soloed = solo === band
    const title = multi || band !== 0 ? BAND_NAMES[band] : 'Whole signal'
    const range = multi ? rangeText(split, band) : band === 0 ? '20 – 20k Hz' : 'Multiband off'
    const width = editor.value(widthId(band))
    const frame = bandCardStyle({ ...split, solo }, band, !active)
    return (
      <Card key={band} title={title} aside={<span className="bd-hint">{range}</span>} minWidth={160} {...frame}>
        <div className="bd-strip">
          <div className="bd-fader-column">
            <LiveCanvas
              className={`bd-bare bd-fader${active ? ' live' : ''}`}
              style={{ width: FADER_W, height: FADER_H }}
              draw={(ctx, w, h) => paintFader(ctx, w, h, width, dragBand(drag.current) === band, active)}
              onPointerDown={
                active
                  ? (_x, y) => {
                      drag.current = { kind: 'fader', band, originY: y, start: widths() }
                    }
                  : undefined
              }
              onPointerMove={
                active
                  ? (_x, y, e) => {
                      const gesture = drag.current
                      if (gesture?.kind !== 'fader' || gesture.band !== band) return
                      if (e.buttons === 0) {
                        drag.current = null
                        return
                      }
                      const travel = Math.max(1, e.currentTarget.clientHeight)
                      const scale = e.shiftKey ? FINE : 1
                      dragWidths(editor, split, linkBands, band, gesture.start, ((gesture.originY - y) / travel) * MAX_WIDTH * scale)
                    }
                  : undefined
              }
              onPointerUp={
                active
                  ? () => {
                      drag.current = null
                    }
                  : undefined
              }
              onDoubleClick={active ? () => resetWidth(editor, split, linkBands, band) : undefined}
            />
            <span className="bd-caption">Width</span>
            <span className="bd-width">{widthReadout(width)}</span>
          </div>
          <LiveCanvas
            className="bd-bare bd-correlation"
            style={{ width: CORRELATION_W + 2 * CORRELATION_PAD, height: FADER_H }}
            draw={(ctx, w, h) => paintCorrelationVertical(ctx, w, h, bandCorrelation(band), CORRELATION_PAD)}
          />
          <div className="bd-strip-controls">
            <BandKnob editor={editor} kind="imager" id={stereoizeId(band)} />
            {multi && (
              <Pill on={soloed} tone="var(--warning)" onClick={() => editor.set('soloBand', soloed ? -1 : band)}>
                Solo
              </Pill>
            )}
          </div>
        </div>
      </Card>
    )
  })

  return (
    <EditorShell
      editor={editor}
      title="Imager"
      subtitle={multi ? 'Four-band stereo imaging' : 'Stereo imaging'}
      keep={KEEP}
    >
      <div className="bd">
        <div className="bd-main bd-imager-main">
          <div className="bd-scope-column">
            <LiveCanvas
              className="bd-scope"
              draw={(ctx, w, h) => {
                // The scope sits below the frame's tags.
                ctx.save()
                ctx.translate(0, 22)
                paintVectorscope(ctx, w, h - 22, live, scope)
                ctx.restore()
              }}
            >
              <DisplayTag>Vectorscope</DisplayTag>
              <DisplayTag right>{SCOPE_MODES.find(([mode]) => mode === scope)![2]}</DisplayTag>
            </LiveCanvas>
            <Choice
              value={scope}
              options={SCOPE_MODES.map(([mode, text]) => [mode, text] as const)}
              onChange={setScope}
              className="bd-scope-modes"
            />
            <Well height={58} draw={(ctx, w, h) => paintCorrelation(ctx, w, h, overall(), true)} />
            <Well height={66} draw={(ctx, w, h) => paintMeterRows(ctx, w, h, live, ['in', 'out'], 6, false)} />
          </div>
          <BandDisplay
            editor={editor}
            kind="imager"
            drag={drag}
            linkBands={linkBands}
            legend={multi ? 'Drag a band up to widen it, a crossover to move it' : 'Drag up to widen the whole signal'}
            notice={notice}
          />
        </div>
        <div className="bd-bottom">
          <Card title="Global" grow={0} minWidth={230}>
            <div className="bd-global">
              <div className="bd-global-flags">
                <ParamCheck editor={editor} id="multiband" label="Multiband" />
                <label className={`pe-check${multi ? '' : ' bd-disabled'}`}>
                  <input
                    type="checkbox"
                    checked={linkBands}
                    disabled={!multi}
                    onChange={(e) => setLinkBands(e.currentTarget.checked)}
                  />
                  <span>Link Bands</span>
                </label>
                <ParamCheck editor={editor} id="recoverSides" label="Recover Sides" />
                <span className="bd-caption">Stereoize</span>
                <ParamChoice
                  editor={editor}
                  id="stereoizeMode"
                  options={[
                    [0, 'I'],
                    [1, 'II'],
                  ]}
                />
                <span className="bd-hint">{stereoizeMode === 1 ? 'Decorrelated: smooth' : 'Haas delay: bold'}</span>
              </div>
              <BandKnob editor={editor} kind="imager" id="outputDb" size={KNOB + 6} />
            </div>
          </Card>
          {strips}
        </div>
      </div>
    </EditorShell>
  )
}

export const editors: Record<string, EditorComponent> = {
  compresser: Compressor,
  imager: Imager,
}
