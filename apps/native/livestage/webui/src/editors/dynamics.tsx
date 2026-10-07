// Editors for FA-2A, FA-76, Z-Comp, BurnLimit, 67Clipper and Transient: a
// port of the native dynamics family (components/dyn_model.rs, dyn_panel.rs).
//
// Each has a row of displays over a row of control cards:
// * the hardware emulations get a moving-coil VU, reading gain reduction or
//   output (a switch in the editor, not a param);
// * Z-Comp, the limiter and the clipper draw their static curve from the DSP
//   crates' own `transfer_db` (ported below), with the live operating point;
// * Transient sketches a hit before and after its shaping;
// * every one keeps ten seconds of input, reduction and output history (or
//   its curve) and a bay of In / reduction / Out meters.

import { useState } from 'react'
import type { CSSProperties, ReactNode } from 'react'
import { Knob } from '../controls.tsx'
import type { Editor, EditorComponent } from './kit.tsx'
import { Card, Choice, DisplayTag, EditorShell, HERO_KNOB, KitKnob, KNOB, LiveCanvas, ParamCheck, ParamChoice, Row } from './kit.tsx'
import type { KnobSpec, Taper, Unit } from './knobspec.ts'
import { bipolar, spec } from './knobspec.ts'
import type { Ctx, Marker, MeterColumn, VuFace, VuScale } from './paint.ts'
import {
  alpha,
  area,
  colors,
  dashed,
  DENSE_CAPTION,
  label,
  line,
  paintHistory,
  paintMeters,
  paintOperatingPoint,
  paintVu,
  rect,
  transferPlot,
  VU_BLUE,
  VU_CREAM,
  vuTinted,
} from './paint.ts'
import './dynamics.css'

type Kind = 'fa2a' | 'fa76' | 'zcomp' | 'burnlimit' | 'clipper67' | 'transient'

const TITLE: Record<Kind, string> = {
  fa2a: 'FA-2A',
  fa76: 'FA-76',
  zcomp: 'Z-Comp',
  burnlimit: 'BurnLimit',
  clipper67: '67Clipper',
  transient: 'Transient',
}

/** zcomp::CompModel::display_name, by wire value. */
const ZCOMP_MODELS = ['2500', 'Distress', 'Avalon', 'SSL']

/** Wire enums decode by rounding; anything out of range is the first. */
function wireEnum(value: number, count: number): number {
  const i = Math.round(value)
  return i >= 0 && i < count ? i : 0
}

function subtitle(kind: Kind, e: Editor): string {
  switch (kind) {
    case 'fa2a':
      return 'Optical leveling amplifier'
    case 'fa76':
      return 'FET limiting amplifier'
    case 'zcomp':
      return `Multi-circuit dynamics · ${ZCOMP_MODELS[wireEnum(e.value('model'), 4)]}`
    case 'burnlimit':
      return 'Loudness maximizer'
    case 'clipper67':
      return 'Oversampled clipper and peak limiter'
    case 'transient':
      return 'Attack and sustain shaper'
  }
}

/** The meter a reduction column reads, and the sign its readout takes. */
function reductionLabel(kind: Kind): [string, string] {
  if (kind === 'clipper67') return ['Clip', '−']
  if (kind === 'transient') return ['Shape', '']
  return ['GR', '−']
}

// ── Knobs (dyn_model::knob) ─────────────────────────────────────────────

type KnobRow = [label: string, taper: Taper, unit: Unit]

const KNOBS: Record<Kind, Record<string, KnobRow>> = {
  fa2a: {
    peakReduction: ['Peak Reduction', 'linear', 'plain'],
    gainDb: ['Gain', 'linear', 'db'],
    emphasis: ['Emphasis', 'linear', 'percent'],
    color: ['Color', 'linear', 'percent'],
    sidechainLowCutHz: ['Sidechain', 'log', 'hz'],
    outputTrimDb: ['Trim', 'linear', 'db'],
  },
  fa76: {
    inputDb: ['Input', 'linear', 'db'],
    outputDb: ['Output', 'linear', 'db'],
    attackUs: ['Attack', 'log', 'us'],
    releaseMs: ['Release', 'log', 'ms'],
    sidechainHpfHz: ['SC HPF', 'square', 'cutHz'],
  },
  zcomp: {
    thresholdDb: ['Thresh', 'linear', 'db'],
    ratio: ['Ratio', 'log', 'ratio'],
    attackMs: ['Attack', 'log', 'ms'],
    releaseMs: ['Release', 'log', 'ms'],
    kneeDb: ['Knee', 'linear', 'db'],
    makeupDb: ['Makeup', 'linear', 'db'],
    sidechainHpfHz: ['SC HPF', 'log', 'hz'],
    stereoLink: ['Link', 'linear', 'percent'],
    color: ['Color', 'linear', 'percent'],
  },
  burnlimit: {
    gainDb: ['Gain', 'linear', 'db'],
    ceilingDb: ['Ceiling', 'linear', 'db'],
    releaseMs: ['Release', 'log', 'ms'],
    lookaheadMs: ['Lookahead', 'linear', 'ms'],
  },
  clipper67: {
    inputDb: ['Input', 'linear', 'db'],
    thresholdDb: ['Threshold', 'linear', 'db'],
    shape: ['Shape', 'linear', 'percent'],
    ceilingDb: ['Ceiling', 'linear', 'db'],
  },
  transient: {
    attack: ['Attack', 'linear', 'percent'],
    sustain: ['Sustain', 'linear', 'percent'],
    speed: ['Speed', 'linear', 'percent'],
  },
}

/** Drawn from where they rest: a trim at unity, a shaper at no change. */
const BIPOLAR: Record<Kind, string[]> = {
  fa2a: ['gainDb', 'outputTrimDb'],
  fa76: [],
  zcomp: ['makeupDb'],
  burnlimit: ['gainDb'],
  clipper67: ['inputDb'],
  transient: ['attack', 'sustain'],
}

/** FA-76's sidechain filter is off at or below this. */
const FA76_HPF_OFF_HZ = 9.5

/** The knob for `id`, its range from the effect's descriptor (so a range
 *  change in the DSP reaches the editor); null for an id the effect lacks. */
function knobSpec(e: Editor, kind: Kind, id: string): KnobSpec | null {
  const range = e.effect.params.find((p) => p.id === id)
  const row: KnobRow | undefined = id === 'mix' ? ['Mix', 'linear', 'percent'] : KNOBS[kind][id]
  if (!range || !row) return null
  let s = spec(id, row[0], range.min, range.max, row[1], row[2])
  if (kind === 'fa76' && id === 'sidechainHpfHz') s = { ...s, cutOff: FA76_HPF_OFF_HZ }
  if (BIPOLAR[kind].includes(id)) s = bipolar(s, 0)
  return s
}

/** native plugin_kit::KNOB_PITCH: a knob cell and its gap. */
const KNOB_PITCH = 62

/** A min-width that never pushes past a phone's width. */
const minWidth = (px: number): CSSProperties => ({ minWidth: `min(${px}px, 100%)` })

// ── Choices (dyn_model::choice_labels, choice_hint) ─────────────────────

const CHOICES: Partial<Record<Kind, [id: string, labels: string[]]>> = {
  fa2a: ['mode', ['Compress', 'Limit']],
  fa76: ['ratio', ['4', '8', '12', '20', 'All']],
  zcomp: ['model', ZCOMP_MODELS],
  burnlimit: ['style', ['Clean', 'Punch', 'Modern', 'Clip']],
  clipper67: ['mode', ['Clip', 'Hybrid', 'Limit']],
}

function choiceHint(kind: Kind, e: Editor): string {
  switch (kind) {
    case 'fa2a':
      return wireEnum(e.value('mode'), 2) === 1 ? 'Higher ratio for peak control' : 'Gentle ratio, slow optical release'
    case 'fa76':
      return wireEnum(e.value('ratio'), 5) === 4
        ? 'All buttons in: hard-knee, aggressive, faster'
        : 'Fixed −24 dB threshold: drive the input into it'
    case 'zcomp':
      return [
        'VCA · feed-forward · thrust sidechain',
        'FET · feedback loop · British grit',
        'Opto · Class-A · over-easy ratio',
        'Bus VCA · dual time-constant release',
      ][wireEnum(e.value('model'), 4)]
    case 'burnlimit':
      return [
        '4 dB knee, 2 ms attack',
        '2 dB knee, 0.8 ms attack',
        '1.5 dB knee, 0.4 ms attack',
        'Near-hard 0.2 dB knee, 0.05 ms attack',
      ][wireEnum(e.value('style'), 4)]
    case 'clipper67':
      return [
        'Clips at the threshold, untouched below the knee',
        'Clips 3 dB past the knee, 1 ms limiter beyond',
        '1 ms lookahead peak limiter at the threshold',
      ][wireEnum(e.value('mode'), 3)]
    case 'transient':
      return ''
  }
}

/** plugin_kit::track_width: segments as wide as the widest label. */
function trackWidth(labels: string[]): number {
  const widest = Math.max(...labels.map((l) => Math.max(40, l.length * 8 + 16)))
  return widest * labels.length + 10
}

// ── The DSP's static curves (ported from the crates) ────────────────────

const dbToLinear = (db: number) => Math.pow(10, db / 20)
const linearToDb = (v: number) => 20 * Math.log10(Math.max(1e-12, v))
const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v))
const blend = (dry: number, wet: number, amount: number) => {
  const a = clamp(amount, 0, 1)
  return dry * (1 - a) + wet * a
}

interface ZcompCurve {
  thresholdDb: number
  ratio: number
  kneeDb: number
  overEasyDb: number
}

/** The static-curve half of zcomp::model_coeffs. */
function zcompCurve(e: Editor): ZcompCurve {
  const color = clamp(e.value('color'), 0, 100) / 100
  const ratio = Math.max(1, e.value('ratio'))
  const knee = Math.max(0, e.value('kneeDb'))
  const threshold = e.value('thresholdDb')
  switch (wireEnum(e.value('model'), 4)) {
    case 1:
      return {
        thresholdDb: threshold - color * 1.5,
        ratio: Math.min(40, ratio * (1 + color * 0.5)),
        kneeDb: Math.max(0.3, knee * (1 - color * 0.6)),
        overEasyDb: 0,
      }
    case 2:
      return {
        thresholdDb: threshold,
        ratio: clamp(2 + (ratio - 1) * 0.55, 1.5, 8),
        kneeDb: Math.min(24, knee + 6 + color * 4),
        overEasyDb: 8,
      }
    case 3:
      return { thresholdDb: threshold, ratio: clamp(ratio, 1.5, 10), kneeDb: clamp(knee + 3, 2, 18), overEasyDb: 0 }
    default:
      return { thresholdDb: threshold, ratio, kneeDb: Math.min(24, knee + 3 + color * 3), overEasyDb: 0 }
  }
}

/** zcomp::curve_reduction_db */
function curveReductionDb(level: number, threshold: number, ratio: number, knee: number, overEasy: number): number {
  const over = level - threshold
  const halfKnee = knee * 0.5
  if (over <= -halfKnee) return 0
  let curved: number
  if (over >= halfKnee || knee <= 1e-4) curved = Math.max(0, over)
  else {
    const t = over + halfKnee
    curved = (t * t) / (2 * knee)
  }
  const effective = overEasy > 0 ? 1 + (ratio - 1) * (curved / (curved + overEasy)) : ratio
  return curved * (1 - 1 / effective)
}

/** burnlimit::static_reduction_db */
function staticReductionDb(level: number, threshold: number, kneeDb: number): number {
  const knee = Math.max(1e-6, kneeDb)
  const start = threshold - knee * 0.5
  if (level <= start) return 0
  if (level >= threshold + knee * 0.5) return level - threshold
  const into = level - start
  return (into * into) / (2 * knee)
}

/** burnlimit::Style::knee_db, by wire value. */
const BURN_KNEE_DB = [4, 2, 1.5, 0.2]

/** clipper67::KNEE_MAX, HYBRID_CLIP_DB, TAPS_PER_PHASE, LOOKAHEAD_SECONDS */
const CLIP_KNEE_MAX = 0.5
const HYBRID_CLIP_DB = 3
const CLIP_OS_TAPS = 32
const CLIP_LOOKAHEAD_MS = 1

/** clipper67::Oversampling, by wire value. */
const CLIP_OVERSAMPLING = ['1×', '2×', '4×', '8×']

/** clipper67::clip_curve: unity to level·(1 − knee), a quadratic bend that
 *  meets the level flat at level·(1 + knee), the level beyond. */
function clipCurve(x: number, level: number, knee: number): number {
  const a = Math.abs(x)
  const start = level * (1 - knee)
  if (a <= start) return x
  let y: number
  if (knee <= 0 || a >= level * (1 + knee)) y = level
  else {
    const u = a - start
    y = a - (u * u) / (4 * level * knee)
  }
  return Math.sign(x) * y
}

/** clipper67::clip_level_db: the threshold, never above the ceiling. */
const clipLevelDb = (e: Editor) => Math.min(e.value('thresholdDb'), e.value('ceilingDb'))

/** What the clipper's settings cost in delay (clipper67::latency_for). */
function clipLatencyHint(e: Editor): string {
  const filters = wireEnum(e.value('oversampling'), 4) > 0
  const lookahead = wireEnum(e.value('mode'), 3) > 0
  if (filters && lookahead) return `Latency ${CLIP_OS_TAPS} samples + ${CLIP_LOOKAHEAD_MS} ms`
  if (filters) return `Latency ${CLIP_OS_TAPS} samples`
  if (lookahead) return `Latency ${CLIP_LOOKAHEAD_MS} ms lookahead`
  return 'Zero latency'
}

/** The steady-state output, in dBFS, of a level held at `input`: the DSP
 *  crates' own `transfer_db`. Null for an effect without one drawn. */
function transferOf(kind: Kind, e: Editor): ((input: number) => number) | null {
  const power = e.flag('power')
  const amount = clamp(e.value('mix'), 0, 100) / 100
  switch (kind) {
    case 'zcomp': {
      const c = zcompCurve(e)
      const makeup = e.value('makeupDb')
      return (input) => {
        if (!power) return input
        const gr = curveReductionDb(input, c.thresholdDb, c.ratio, c.kneeDb, c.overEasyDb)
        const dry = dbToLinear(input)
        return linearToDb(Math.max(1e-9, blend(dry, dry * dbToLinear(makeup - gr), amount)))
      }
    }
    case 'burnlimit': {
      const gain = e.value('gainDb')
      const ceiling = e.value('ceilingDb')
      const knee = BURN_KNEE_DB[wireEnum(e.value('style'), 4)]
      return (input) => {
        if (!power) return input
        const driven = input + gain
        const wet = Math.min(ceiling, driven - staticReductionDb(driven, ceiling, knee))
        return linearToDb(Math.max(1e-9, blend(dbToLinear(input), dbToLinear(wet), amount)))
      }
    }
    case 'clipper67': {
      // clipper67::transfer_db
      const gain = dbToLinear(e.value('inputDb'))
      const level = dbToLinear(clipLevelDb(e))
      const knee = (clamp(e.value('shape'), 0, 100) / 100) * CLIP_KNEE_MAX
      const ceiling = dbToLinear(e.value('ceilingDb'))
      const hybridTarget = level * (1 + knee) * dbToLinear(HYBRID_CLIP_DB)
      const mode = wireEnum(e.value('mode'), 3)
      return (input) => {
        if (!power) return input
        const dry = dbToLinear(input)
        const driven = dry * gain
        let wet: number
        if (mode === 0) wet = clipCurve(driven, level, knee)
        else if (mode === 1) wet = clipCurve(Math.min(driven, hybridTarget), level, knee)
        else wet = Math.min(driven, level)
        return linearToDb(Math.max(1e-9, blend(dry, Math.min(wet, ceiling), amount)))
      }
    }
    default:
      return null
  }
}

/** The input range a transfer display spans, to 0 dBFS. */
const transferRange = (kind: Kind) => (kind === 'zcomp' ? -60 : -24)

// ── Painters ────────────────────────────────────────────────────────────

/** Room for a display's tags above what it paints. */
const TAG_ROOM = 26

let warningColor: string | null = null
/** The page's warning accent (the native accent_warning). */
function warning(): string {
  warningColor ??= getComputedStyle(document.documentElement).getPropertyValue('--warning').trim() || '#e8b75c'
  return warningColor
}

interface ActionLine {
  db: number
  vertical: boolean
  color: string
}

/** dyn_panel::paint_transfer: the grid, unity, where the action starts, and
 *  the static curve. */
function paintTransfer(
  ctx: Ctx,
  w: number,
  h: number,
  transfer: (input: number) => number,
  range: number,
  actions: ActionLine[],
  bypassed: boolean,
) {
  const c = colors()
  const [x0, y0, pw, ph] = transferPlot(w, h)
  const xAt = (db: number) => x0 + ((db - range) / -range) * pw
  const yAt = (db: number) => y0 + ph - ((clamp(db, range, 0) - range) / -range) * ph
  const step = range < -30 ? 12 : 6
  for (let db = 0; db >= range - 0.01; db -= step) {
    rect(ctx, xAt(db), y0, 1, ph, alpha(c.text, 0.06))
    rect(ctx, x0, yAt(db), pw, 1, alpha(c.text, 0.06))
    if (db < 0 && db > range) {
      const text = db.toFixed(0)
      label(ctx, text, DENSE_CAPTION, c.textFaint, xAt(db), y0 + ph + 4, 'center')
      label(ctx, text, DENSE_CAPTION, c.textFaint, x0 - 6, yAt(db) - 6, 'right')
    }
  }
  dashed(ctx, [x0, y0 + ph], [x0 + pw, y0], 1, alpha(c.text, 0.22))
  for (const action of actions) {
    const color = alpha(action.color, 0.7)
    if (action.vertical) dashed(ctx, [xAt(action.db), y0], [xAt(action.db), y0 + ph], 1, color)
    else dashed(ctx, [x0, yAt(action.db)], [x0 + pw, yAt(action.db)], 1, color)
  }
  const points = Array.from({ length: 121 }, (_, i): [number, number] => {
    const input = range - (range * i) / 120
    return [xAt(input), yAt(transfer(input))]
  })
  const a = bypassed ? 0.35 : 1
  area(ctx, points, y0 + ph, alpha(c.accent, 0.1 * a))
  line(ctx, points, 2, alpha(c.accent, a))
  label(ctx, 'In dB →', DENSE_CAPTION, c.textFaint, x0 + pw, y0 + ph + 4, 'right')
}

/** transient::MAX_SHAPE_DB */
const MAX_SHAPE_DB = 18

/** dyn_panel::paint_envelope: a sketch of a hit before and after — the dry
 *  envelope dashed, the shaped one over it. A picture of the settings, not a
 *  measurement. */
function paintEnvelope(ctx: Ctx, w: number, h: number, attack: number, sustain: number, bypassed: boolean) {
  const c = colors()
  const [x0, y0, pw, ph] = [10, TAG_ROOM + 6, w - 20, h - TAG_ROOM - 16]
  if (pw <= 0 || ph <= 0) return
  const gain = (amount: number) => Math.pow(10, ((amount / 100) * MAX_SHAPE_DB) / 20)
  const [attackGain, sustainGain] = [gain(attack), gain(sustain)]
  const scale = ph / 1.6
  const points = (shaped: boolean) =>
    Array.from({ length: 121 }, (_, i): [number, number] => {
      const t = i / 120
      const env = t < 0.05 ? t / 0.05 : Math.exp(-(t - 0.05) / 0.35)
      const mixTo = clamp((t - 0.1) / 0.15, 0, 1)
      const g = shaped && !bypassed ? attackGain * (1 - mixTo) + sustainGain * mixTo : 1
      return [x0 + t * pw, y0 + ph - Math.min(1.6, env * g) * scale]
    })
  rect(ctx, x0, y0 + ph - scale, pw, 1, alpha(c.text, 0.08))
  const dry = points(false)
  for (let i = 0; i + 1 < dry.length; i += 2) line(ctx, [dry[i], dry[i + 1]], 1, alpha(c.text, 0.35))
  const shaped = points(true)
  area(ctx, shaped, y0 + ph, alpha(c.accent, 0.12))
  line(ctx, shaped, 2, c.accent)
}

/** The VU face a kind wears; Z-Comp's follows its circuit. */
function vuFace(kind: Kind, e: Editor): VuFace {
  if (kind === 'fa2a') return VU_CREAM
  if (kind !== 'zcomp') return VU_BLUE
  switch (wireEnum(e.value('model'), 4)) {
    case 1:
      return vuTinted('#dfb873', '#765221', '#1d1509', '#8f2517')
    case 2:
      return vuTinted('#9bc2a0', '#466149', '#101710', '#8d3419')
    case 3:
      return vuTinted('#4076b4', '#17406e', '#f2f6fa', '#d8492f')
    default:
      return vuTinted('#66bac3', '#1d5963', '#f2f6fa', '#d8492f')
  }
}

/** History markers: the levels the plug-in holds the output to. */
function markers(kind: Kind, e: Editor): Marker[] {
  const ceiling = (): Marker => {
    const db = e.value('ceilingDb')
    return { db, label: `Ceiling ${db.toFixed(1)} dB`, color: warning() }
  }
  if (kind === 'burnlimit') return [ceiling()]
  if (kind === 'clipper67') {
    // The threshold only marks a level when it sits under the ceiling.
    const db = e.value('thresholdDb')
    if (db >= e.value('ceilingDb')) return [ceiling()]
    return [{ db, label: `Threshold ${db.toFixed(1)} dB`, color: colors().accent }, ceiling()]
  }
  return []
}

// ── Displays ────────────────────────────────────────────────────────────

function Displays(props: { editor: Editor; kind: Kind; vuOutput: boolean }) {
  const { editor: e, kind, vuOutput } = props
  const live = e.live
  const bypassed = e.bypassed
  const [reduction, sign] = reductionLabel(kind)

  const column = (grow: number, min: number, child: ReactNode, key: string) => (
    <div key={key} className="dyn-col" style={{ flexGrow: grow, ...minWidth(min) }}>
      {child}
    </div>
  )

  const vu = () => {
    const scale: VuScale = vuOutput ? { kind: 'output' } : { kind: 'reduction', full: kind === 'zcomp' ? 24 : 20 }
    const title = vuOutput ? 'VU' : 'GAIN REDUCTION dB'
    const face = vuFace(kind, e)
    return column(
      3,
      250,
      <div className="dyn-well">
        <LiveCanvas className="dyn-bare" draw={(ctx, w, h) => paintVu(ctx, w, h, live, face, scale, title)} />
      </div>,
      'vu',
    )
  }

  const history = () => {
    const marks = markers(kind, e)
    const notice = e.bypassed ? `Bypassed — ${TITLE[kind]} passes audio through unchanged` : null
    return column(
      4,
      260,
      <LiveCanvas
        draw={(ctx, w, h) => {
          ctx.translate(0, TAG_ROOM)
          paintHistory(ctx, w, h - TAG_ROOM, live, marks, reduction, bypassed)
        }}
      >
        <DisplayTag>Level history</DisplayTag>
        <DisplayTag right>10 s</DisplayTag>
        {notice && <span className="dyn-notice">{notice}</span>}
      </LiveCanvas>,
      'history',
    )
  }

  const transfer = () => {
    const curve = transferOf(kind, e)!
    const range = transferRange(kind)
    // BurnLimit meters its input after the drive.
    const offset = kind === 'burnlimit' ? e.value('gainDb') : 0
    let legend: string
    let actions: ActionLine[]
    if (kind === 'zcomp') {
      const c = zcompCurve(e)
      legend = `${c.thresholdDb.toFixed(1)} dB · ${c.ratio.toFixed(1)}:1`
      actions = [{ db: c.thresholdDb, vertical: true, color: colors().accent }]
    } else {
      const ceiling = e.value('ceilingDb')
      legend = `Ceiling ${ceiling.toFixed(1)} dB`
      actions = [{ db: ceiling, vertical: false, color: warning() }]
      if (kind === 'clipper67') {
        legend = `Clip ${clipLevelDb(e).toFixed(1)} dB · ${CLIP_OVERSAMPLING[wireEnum(e.value('oversampling'), 4)]}`
        // A threshold under the ceiling is where it clips.
        const threshold = e.value('thresholdDb')
        if (threshold < ceiling) actions.push({ db: threshold, vertical: false, color: colors().accent })
      }
    }
    return column(
      2,
      220,
      <LiveCanvas
        draw={(ctx, w, h) => {
          paintTransfer(ctx, w, h, curve, range, actions, bypassed)
          if (!bypassed) paintOperatingPoint(ctx, w, h, live, range, offset, curve)
        }}
      >
        <DisplayTag>Transfer</DisplayTag>
        <DisplayTag right>{legend}</DisplayTag>
      </LiveCanvas>,
      'transfer',
    )
  }

  const envelope = () => {
    const attack = e.value('attack')
    const sustain = e.value('sustain')
    const describe = (amount: number) => {
      const db = (amount / 100) * MAX_SHAPE_DB
      return Math.abs(db) < 0.05 ? 'unchanged' : `${db >= 0 ? '+' : '-'}${Math.abs(db).toFixed(1)} dB`
    }
    return column(
      2,
      220,
      <LiveCanvas draw={(ctx, w, h) => paintEnvelope(ctx, w, h, attack, sustain, bypassed)}>
        <DisplayTag>Shape</DisplayTag>
        <DisplayTag right>{`Attack ${describe(attack)} · Sustain ${describe(sustain)}`}</DisplayTag>
      </LiveCanvas>,
      'shape',
    )
  }

  const meterColumns: MeterColumn[] = [{ kind: 'input' }, { kind: 'reduction', caption: reduction, sign }, { kind: 'output' }]
  const meters = (
    <div key="meters" className="dyn-col dyn-meters">
      <div className="dyn-well">
        <LiveCanvas
          className="dyn-bare"
          draw={(ctx, w, h) => paintMeters(ctx, w, h, live, meterColumns, bypassed)}
        />
      </div>
    </div>
  )

  let displays: ReactNode[]
  switch (kind) {
    case 'fa2a':
    case 'fa76':
      displays = [vu(), history()]
      break
    case 'zcomp':
      displays = [vu(), transfer()]
      break
    case 'burnlimit':
    case 'clipper67':
      displays = [history(), transfer()]
      break
    case 'transient':
      displays = [history(), envelope()]
      break
  }
  return (
    <div className="dyn-displays">
      {displays}
      {meters}
    </div>
  )
}

// ── Controls ────────────────────────────────────────────────────────────

function Caption(props: { children: ReactNode }) {
  return <span className="dyn-caption">{props.children}</span>
}

/** A knob the setting leaves nothing to do: greyed, `why` in place of its
 *  value (plugin_kit::knob_for with a reason). */
function IdleKnob(props: { label: string; why: string; size: number }) {
  return (
    <div className="pe-knob dyn-idle" aria-disabled>
      <Knob
        value={0}
        min={0}
        max={1}
        defaultValue={0}
        size={props.size}
        label={`${props.label}: ${props.why}`}
        caption={props.label}
        format={() => props.why}
        onChange={() => {}}
      />
    </div>
  )
}

function Controls(props: { editor: Editor; kind: Kind; vuOutput: boolean; setVuOutput: (on: boolean) => void }) {
  const { editor: e, kind } = props

  const knob = (id: string, size = KNOB, why?: string) => {
    const s = knobSpec(e, kind, id)
    if (!s) return null
    if (why) return <IdleKnob key={id} label={s.label} why={why} size={size} />
    return <KitKnob key={id} editor={e} spec={s} size={size} />
  }
  const hero = (id: string) => knob(id, HERO_KNOB)

  /** plugin_kit::card */
  const card = (title: string, grow: number, min: number, body: ReactNode) => (
    <Card key={title} title={title} grow={grow} style={minWidth(min)}>
      {body}
    </Card>
  )
  /** plugin_kit::knob_card: as wide as its knobs need. */
  const knobCard = (title: string, knobs: ReactNode[]) =>
    card(title, knobs.length, knobs.length * KNOB_PITCH + 18, <div className="pe-knobs">{knobs}</div>)

  /** The segmented choice of the kind's mode, with what it does under it. */
  const modeCard = (title: string) => {
    const [id, labels] = CHOICES[kind]!
    return card(
      title,
      0,
      trackWidth(labels) + 18,
      <div className="dyn-stack">
        <ParamChoice editor={e} id={id} options={labels.map((l, i): [number, string] => [i, l])} className="dyn-track" />
        <span className="dyn-hint">{choiceHint(kind, e)}</span>
      </div>,
    )
  }

  /** The VU's scale switch: gain reduction or output, under its own caption
   *  unless the card it sits in already says what it is. */
  const vuSwitch = (captioned: boolean) => (
    <div className="dyn-stack hair">
      {captioned && <Caption>METER</Caption>}
      <Choice
        value={props.vuOutput ? 1 : 0}
        options={[
          [0, 'GR'],
          [1, '+4'],
        ]}
        onChange={(v) => props.setVuOutput(v === 1)}
      />
    </div>
  )

  switch (kind) {
    case 'fa2a':
      return (
        <Row>
          {card(
            'LEVELING',
            3,
            360,
            <div className="dyn-cluster">
              {hero('gainDb')}
              {hero('peakReduction')}
              <div className="dyn-stack">
                <Caption>MODE</Caption>
                <ParamChoice
                  editor={e}
                  id="mode"
                  options={[
                    [0, 'Compress'],
                    [1, 'Limit'],
                  ]}
                />
                {vuSwitch(true)}
              </div>
            </div>,
          )}
          {knobCard('CHARACTER', [knob('emphasis'), knob('sidechainLowCutHz'), knob('color')])}
          {knobCard('OUTPUT', [knob('mix'), knob('outputTrimDb')])}
        </Row>
      )
    case 'fa76':
      return (
        <Row>
          {modeCard('RATIO')}
          {card('METER', 0, 110, vuSwitch(false))}
          {knobCard('GAIN', [hero('inputDb'), hero('outputDb')])}
          {knobCard('TIMING', [knob('attackUs'), knob('releaseMs')])}
          {knobCard('SIDECHAIN · MIX', [knob('sidechainHpfHz'), knob('mix')])}
        </Row>
      )
    case 'zcomp': {
      // SSL's auto release is a fixed dual time-constant network: the
      // Release knob has nothing to do.
      const programmed = wireEnum(e.value('model'), 4) === 3 && e.flag('autoRelease')
      return (
        <>
          <Row>
            {modeCard('CIRCUIT')}
            {card(
              'MODES',
              0,
              140,
              <div className="dyn-stack">
                <ParamCheck editor={e} id="autoRelease" label="Auto release" />
                <ParamCheck editor={e} id="scListen" label="SC Listen" />
                {vuSwitch(true)}
              </div>,
            )}
            {knobCard('DETECTOR', [knob('sidechainHpfHz'), knob('stereoLink'), knob('color')])}
          </Row>
          <Row>
            {knobCard('COMPRESSION', [
              knob('thresholdDb'),
              knob('ratio'),
              knob('attackMs'),
              knob('releaseMs', KNOB, programmed ? 'Auto' : undefined),
              knob('kneeDb'),
            ])}
            {knobCard('GAIN', [knob('makeupDb'), knob('mix')])}
          </Row>
        </>
      )
    }
    case 'burnlimit':
      return (
        <Row>
          {modeCard('STYLE')}
          {knobCard('LOUDNESS', [hero('gainDb'), hero('ceilingDb')])}
          {knobCard('TIMING', [knob('releaseMs'), knob('lookaheadMs')])}
          {card(
            'OUTPUT',
            2,
            KNOB_PITCH + 150,
            <div className="dyn-cluster top">
              {knob('mix')}
              <div className="dyn-stack">
                <ParamCheck editor={e} id="truePeak" label="True Peak" />
                <ParamCheck editor={e} id="stereoLink" label="Stereo Link" />
              </div>
            </div>,
          )}
        </Row>
      )
    case 'clipper67': {
      // Limit ignores the knee: Shape has nothing to do.
      const limiting = wireEnum(e.value('mode'), 3) === 2
      return (
        <Row>
          {modeCard('MODE')}
          {card(
            'DRIVE · CLIP',
            3,
            3 * KNOB_PITCH + 60,
            <div className="dyn-stack hair">
              <div className="dyn-cluster tight">
                {hero('inputDb')}
                {hero('thresholdDb')}
                {knob('shape', HERO_KNOB, limiting ? 'Limit' : undefined)}
              </div>
              <span className="dyn-hint">Unity under the knee · Shape 0 % hard, 100 % soft</span>
            </div>,
          )}
          {knobCard('OUTPUT', [knob('ceilingDb'), knob('mix')])}
          {card(
            'OVERSAMPLING',
            1,
            trackWidth(CLIP_OVERSAMPLING) + 18,
            <div className="dyn-stack">
              <ParamChoice
                editor={e}
                id="oversampling"
                options={CLIP_OVERSAMPLING.map((l, i): [number, string] => [i, l])}
                className="dyn-track"
              />
              <span className="dyn-hint">{clipLatencyHint(e)}</span>
              <ParamCheck editor={e} id="dcFilter" label="DC Filter" />
              <ParamCheck editor={e} id="stereoLink" label="Stereo Link" />
              <ParamCheck editor={e} id="delta" label="Delta (what is clipped)" />
            </div>,
          )}
        </Row>
      )
    }
    case 'transient':
      return (
        <Row>
          {knobCard('SHAPE', [hero('attack'), hero('sustain')])}
          {knobCard('DETECTOR', [knob('speed')])}
          {card(
            'OUTPUT',
            2,
            KNOB_PITCH + 150,
            <div className="dyn-cluster top">
              {knob('mix')}
              <ParamCheck editor={e} id="stereoLink" label="Stereo Link" />
            </div>,
          )}
        </Row>
      )
  }
}

// ── The editor ──────────────────────────────────────────────────────────

function editorFor(kind: Kind): EditorComponent {
  return function DynamicsEditor(props: { editor: Editor }) {
    const { editor } = props
    // Which scale the VU reads: the editor's own, not a param.
    const [vuOutput, setVuOutput] = useState(false)
    return (
      <EditorShell
        editor={editor}
        title={TITLE[kind]}
        subtitle={subtitle(kind, editor)}
        // SC Listen is a listening aid, not a sound: a preset leaves it. So
        // do the clipper's oversampling (a latency choice) and Delta.
        keep={kind === 'zcomp' ? ['scListen'] : kind === 'clipper67' ? ['oversampling', 'delta'] : undefined}
      >
        <Displays editor={editor} kind={kind} vuOutput={vuOutput} />
        <Controls editor={editor} kind={kind} vuOutput={vuOutput} setVuOutput={setVuOutput} />
      </EditorShell>
    )
  }
}

export const editors: Record<string, EditorComponent> = {
  fa2a: editorFor('fa2a'),
  fa76: editorFor('fa76'),
  zcomp: editorFor('zcomp'),
  burnlimit: editorFor('burnlimit'),
  clipper67: editorFor('clipper67'),
  transient: editorFor('transient'),
}
