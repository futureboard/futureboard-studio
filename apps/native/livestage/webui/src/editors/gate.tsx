// WayGate's editor: a port of the native one (components/gate_model.rs,
// gate_panel.rs).
//
// A row of displays over a row of control cards:
// * Gate history: ten seconds of the input (shaded), the key the detector
//   hears and the output, under the open and close thresholds with the
//   hysteresis band between them, and a lane showing how far open the gate
//   is and when the detector is triggered;
// * Response: the static in/out curve as the hysteresis loop it is, with the
//   live operating point;
// * Meters: In, Key, the gate's gain over its range, Out, and the detector's
//   state as a lamp and a word.
//
// The key and the detector state arrive in the standard level frame, on rack
// position 0 (`waygate::KEY_SLOT`).

import type { CSSProperties, ReactNode } from 'react'
import { Knob } from '../controls.tsx'
import type { Editor, EditorComponent } from './kit.tsx'
import { Card, DisplayTag, EditorShell, HERO_KNOB, KNOB, LiveCanvas, ParamCheck, ParamChoice, Row } from './kit.tsx'
import type { KnobSpec, Taper, Unit } from './knobspec.ts'
import { formatValue, fromKnob, knobRange, spec, toKnob } from './knobspec.ts'
import type { HistoryPoint, Live } from './live.ts'
import { HISTORY, toDb } from './live.ts'
import type { Ctx } from './paint.ts'
import { alpha, area, colors, dashed, DENSE_CAPTION, dot, label, line, rect, transferPlot, UI_XS } from './paint.ts'
import './gate.css'

/** waygate::RANGE_FLOOR_DB: the threshold's floor, and a full mute. */
const FLOOR_DB = -80
/** waygate::KEY_HPF_OFF_HZ / KEY_LPF_OFF_HZ */
const KEY_HPF_OFF_HZ = 20
const KEY_LPF_OFF_HZ = 20000
/** Room for a display's tags above what it paints. */
const TAG_ROOM = 26
/** native plugin_kit::KNOB_PITCH: a knob cell and its gap. */
const KNOB_PITCH = 62

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v))
const isFullMute = (rangeDb: number) => rangeDb <= FLOOR_DB + 1e-3
/** A min-width that never pushes past a phone's width. */
const minWidth = (px: number): CSSProperties => ({ minWidth: `min(${px}px, 100%)` })

function dbText(db: number): string {
  return Math.abs(db) < 0.05 ? '0.0' : db.toFixed(1)
}

// ── Knobs (gate_model::knob, readout) ───────────────────────────────────

const KNOBS: Record<string, [label: string, taper: Taper, unit: Unit]> = {
  thresholdDb: ['Threshold', 'linear', 'db'],
  rangeDb: ['Range', 'linear', 'db'],
  hysteresisDb: ['Hysteresis', 'linear', 'db'],
  attackMs: ['Attack', 'log', 'ms'],
  holdMs: ['Hold', 'square', 'ms'],
  releaseMs: ['Release', 'log', 'ms'],
  keyHpfHz: ['Key HPF', 'log', 'cutHz'],
  keyLpfHz: ['Key LPF', 'log', 'hz'],
  lookaheadMs: ['Lookahead', 'linear', 'ms'],
}

/** The knob for `id`, its range from the effect's descriptor. */
function knobSpec(e: Editor, id: string): KnobSpec | null {
  const range = e.effect.params.find((p) => p.id === id)
  const row = KNOBS[id]
  if (!range || !row) return null
  const s = spec(id, row[0], range.min, range.max, row[1], row[2])
  return id === 'keyHpfHz' ? { ...s, cutOff: KEY_HPF_OFF_HZ } : s
}

/** The gate's own readouts where the generic one would mislead: a mute is
 *  −∞, a filter at its end stop is off, hysteresis is a distance. */
function readout(s: KnobSpec, value: number): string {
  switch (s.id) {
    case 'rangeDb':
      return isFullMute(value) ? '−∞ dB' : `${dbText(value)} dB`
    case 'thresholdDb':
      return `${dbText(value)} dB`
    case 'hysteresisDb':
      return `${value.toFixed(1)} dB`
    case 'keyLpfHz':
      return value >= KEY_LPF_OFF_HZ - 0.5 ? 'Off' : formatValue('hz', value)
    case 'lookaheadMs':
      return value <= 0 ? 'Off' : formatValue('ms', value)
    case 'holdMs':
      return value <= 0 ? '0 ms' : formatValue('ms', value)
    default:
      return formatValue(s.unit, value, s.cutOff ?? 0)
  }
}

// ── The response (gate_model::response_db, opens_at) ────────────────────

interface Gate {
  power: boolean
  duck: boolean
  thresholdDb: number
  rangeDb: number
  hysteresisDb: number
}

function gateOf(e: Editor): Gate {
  return {
    power: e.flag('power'),
    duck: Math.round(e.value('mode')) === 1,
    thresholdDb: e.value('thresholdDb'),
    rangeDb: e.value('rangeDb'),
    hysteresisDb: e.value('hysteresisDb'),
  }
}

function opensAt(g: Gate, keyDb: number, rising: boolean): boolean {
  return rising ? keyDb >= g.thresholdDb : keyDb >= g.thresholdDb - g.hysteresisDb
}

function responseDb(g: Gate, keyDb: number, open: boolean): number {
  if (!g.power) return keyDb
  const closed = g.duck ? open : !open
  return keyDb + (closed ? clamp(g.rangeDb, FLOOR_DB, 0) : 0)
}

function responseAxes(w: number, h: number) {
  const [x0, y0, pw, ph] = transferPlot(w, h)
  const xAt = (db: number) => x0 + ((clamp(db, FLOOR_DB, 0) - FLOOR_DB) / -FLOOR_DB) * pw
  const yAt = (db: number) => y0 + ph - ((clamp(db, FLOOR_DB, 0) - FLOOR_DB) / -FLOOR_DB) * ph
  return { x0, y0, pw, ph, xAt, yAt }
}

function paintResponse(ctx: Ctx, w: number, h: number, g: Gate, live: Live, bypassed: boolean) {
  const c = colors()
  const { x0, y0, pw, ph, xAt, yAt } = responseAxes(w, h)
  for (let db = 0; db >= FLOOR_DB - 0.01; db -= 20) {
    rect(ctx, xAt(db), y0, 1, ph, alpha(c.text, 0.06))
    rect(ctx, x0, yAt(db), pw, 1, alpha(c.text, 0.06))
    if (db < 0 && db > FLOOR_DB) {
      label(ctx, db.toFixed(0), DENSE_CAPTION, c.textFaint, xAt(db), y0 + ph + 4, 'center')
      label(ctx, db.toFixed(0), DENSE_CAPTION, c.textFaint, x0 - 6, yAt(db) - 6, 'right')
    }
  }
  dashed(ctx, [x0, y0 + ph], [x0 + pw, y0], 1, alpha(c.text, 0.22))
  const a = bypassed ? 0.35 : 1
  const banded = g.power && g.hysteresisDb > 0.05
  if (banded) {
    const left = xAt(g.thresholdDb - g.hysteresisDb)
    rect(ctx, left, y0, xAt(g.thresholdDb) - left, ph, alpha(c.accent, 0.08 * a))
  }
  const branch = (rising: boolean) =>
    Array.from({ length: 321 }, (_, i): [number, number] => {
      const key = FLOOR_DB - (FLOOR_DB * i) / 320
      return [xAt(key), yAt(responseDb(g, key, opensAt(g, key, rising)))]
    })
  const rising = branch(true)
  area(ctx, rising, y0 + ph, alpha(c.accent, 0.1 * a))
  line(ctx, rising, 2, alpha(c.accent, a))
  if (banded) {
    const falling = branch(false)
    for (let i = 0; i + 1 < falling.length; i += 2) line(ctx, [falling[i], falling[i + 1]], 1.5, alpha(c.accent, 0.6 * a))
  }
  label(ctx, 'Key dB →', DENSE_CAPTION, c.textFaint, x0 + pw, y0 + ph + 4, 'right')

  // The operating point: the key against what comes out at it.
  const frame = live.frame
  const key = frame?.slot_in_peak[0] ?? 0
  if (bypassed || !frame || key <= 1e-5) return
  const keyDb = toDb(key)
  const x = xAt(keyDb)
  const y = yAt(keyDb - Math.max(0, frame.gain_reduction_db))
  dashed(ctx, [x, y0 + ph], [x, y], 1, alpha(c.accentHover, 0.55))
  dot(ctx, x, y, 4, c.accentHover)
}

// ── The history (gate_panel::paint_gate_history) ────────────────────────

/** How open the gate is at a point: its reduction over the range. */
function openness(p: HistoryPoint, g: Gate): number {
  const span = -clamp(g.rangeDb, FLOOR_DB, 0)
  return span < 0.05 ? 1 : 1 - clamp(p.reductionDb / span, 0, 1)
}

function paintGateHistory(ctx: Ctx, w: number, h: number, live: Live, g: Gate, bypassed: boolean) {
  const c = colors()
  const GUTTER = 34
  const PAD_Y = 8
  const LANE_H = 14
  const LANE_GAP = 6
  const LEGEND_H = 16
  const plotW = Math.max(1, w - GUTTER)
  const laneY = h - LEGEND_H - LANE_H
  const plotTop = PAD_Y
  const plotH = Math.max(1, laneY - LANE_GAP - PAD_Y)
  const yAt = (db: number) => plotTop + clamp(db / FLOOR_DB, 0, 1) * plotH
  const xAt = (age: number) => plotW - (age * plotW) / (HISTORY - 1)
  for (const db of [0, -12, -24, -36, -48, -60, -72]) {
    const y = yAt(db)
    rect(ctx, 0, y, plotW, 1, alpha(c.text, 0.06))
    label(ctx, db.toFixed(0), DENSE_CAPTION, c.textFaint, plotW + 6, y - 6)
  }
  const a = bypassed ? 0.45 : 1
  const closeDb = g.thresholdDb - g.hysteresisDb
  const banded = g.hysteresisDb > 0.05
  if (banded) rect(ctx, 0, yAt(g.thresholdDb), plotW, yAt(closeDb) - yAt(g.thresholdDb), alpha(c.accent, 0.07 * a))

  if (live.count > 1) {
    const ages = Array.from({ length: live.count }, (_, i) => i)
    const points = (read: (p: HistoryPoint) => number) =>
      ages.map((age): [number, number] => [xAt(age), yAt(read(live.point(age)))])
    const input = points((p) => p.inDb)
    area(ctx, input, plotTop + plotH, alpha(c.textMuted, 0.2 * a))
    line(ctx, input, 1, alpha(c.textMuted, 0.6 * a))
    line(ctx, points((p) => p.slotInDb), 1, alpha(c.textSecondary, 0.85 * a))
    line(ctx, points((p) => p.outDb), 1.25, alpha(c.text, 0.9 * a))
  }

  const open = alpha(c.accent, a)
  const yOpen = yAt(g.thresholdDb)
  dashed(ctx, [0, yOpen], [plotW, yOpen], 1, open)
  label(ctx, `Open ${dbText(g.thresholdDb)} dB`, DENSE_CAPTION, open, 8, yOpen - 14 < 4 ? yOpen + 3 : yOpen - 14)
  if (banded) {
    const close = alpha(c.accent, 0.6 * a)
    const yClose = yAt(closeDb)
    dashed(ctx, [0, yClose], [plotW, yClose], 1, close)
    label(ctx, `Close ${dbText(closeDb)} dB`, DENSE_CAPTION, close, yClose - yOpen < 16 ? 110 : 8, yClose + 3)
  }

  // The gate lane: how open it is, and a bar while triggered.
  rect(ctx, 0, laneY, plotW, LANE_H, c.meterBg)
  const columnW = plotW / (HISTORY - 1)
  for (let age = 0; age < live.count; age++) {
    const p = live.point(age)
    const o = bypassed ? 1 : openness(p, g)
    const x = xAt(age) - columnW
    if (o > 0.01) rect(ctx, x, laneY + LANE_H - o * LANE_H, columnW + 0.5, o * LANE_H, alpha(c.accent, 0.55 * a))
    if (p.slotLit && !bypassed) rect(ctx, x, laneY, columnW + 0.5, 2, c.text)
  }
  label(ctx, g.duck ? 'DUCK' : 'GATE', DENSE_CAPTION, c.textFaint, plotW + 6, laneY + 1)
  label(ctx, 'In · Key · Out · lane: gain, bar: triggered', DENSE_CAPTION, c.textFaint, 8, laneY + LANE_H + 2)
}

// ── Meters (gate_panel::paint_gate_meters) ──────────────────────────────

function paintGateMeters(ctx: Ctx, w: number, h: number, live: Live, g: Gate, bypassed: boolean) {
  const c = colors()
  const STATE_H = 22
  const CAPTION_H = 16
  const READOUT_H = 18
  const BAR_W = 12
  const has = live.frame !== null && live.count > 0

  // The detector's state: a lamp and a word, so it never rests on hue.
  const triggered = has && !bypassed && live.point(0).slotLit
  const word = bypassed
    ? 'Bypassed'
    : !has
      ? 'No signal'
      : g.duck
        ? triggered
          ? 'Ducking'
          : 'Idle'
        : triggered
          ? 'Open'
          : 'Closed'
  dot(ctx, 6, 7, 4, triggered ? c.accent : alpha(c.textFaint, 0.6))
  label(ctx, word, UI_XS, triggered ? c.text : c.textMuted, 16, 0)

  const columnW = w / 4
  const barTop = STATE_H + CAPTION_H
  const barH = Math.max(1, h - READOUT_H - barTop)
  const levelUnit = (db: number) => clamp((db - FLOOR_DB) / -FLOOR_DB, 0, 1)
  const span = -clamp(g.rangeDb, FLOOR_DB, 0)
  ;['In', 'Key', 'Gain', 'Out'].forEach((caption, i) => {
    const centre = columnW * (i + 0.5)
    label(ctx, caption, DENSE_CAPTION, c.textMuted, centre, STATE_H, 'center')
    const x = centre - BAR_W / 2
    rect(ctx, x, barTop, BAR_W, barH, c.meterBg)
    let text: string
    if (i === 2) {
      // The gain, hanging from the top over the range.
      const now = has && !bypassed ? live.point(0).reductionDb : 0
      const unit = span > 0.05 ? clamp(now / span, 0, 1) : 0
      rect(ctx, x, barTop, BAR_W, unit * barH, c.accent)
      text = !has ? '—' : now < 0.05 ? '0.0' : isFullMute(g.rangeDb) && now >= span - 0.05 ? '−∞' : `−${now.toFixed(1)}`
    } else {
      const read = (p: HistoryPoint) => (i === 0 ? p.inDb : i === 1 ? p.slotInDb : p.outDb)
      const now = has ? read(live.point(0)) : -120
      const held = has ? live.held(read) : -120
      const unit = levelUnit(now)
      const top = barTop + (1 - unit) * barH
      rect(ctx, x, top, BAR_W, barTop + barH - top, unit >= 1 ? c.meterHigh : c.textMuted)
      const hold = levelUnit(held)
      if (hold > 0) rect(ctx, x, barTop + (1 - hold) * barH, BAR_W, 1.5, c.text)
      // The thresholds the key is compared with, on the In and Key bars.
      if (i < 2 && !bypassed) {
        rect(ctx, x - 3, barTop + (1 - levelUnit(g.thresholdDb)) * barH, BAR_W + 6, 1, c.accent)
        if (g.hysteresisDb > 0.05) {
          const closeY = barTop + (1 - levelUnit(g.thresholdDb - g.hysteresisDb)) * barH
          rect(ctx, x - 3, closeY, BAR_W + 6, 1, alpha(c.accent, 0.55))
        }
      }
      text = !has || held <= -119 ? '—' : dbText(held)
    }
    label(ctx, text, UI_XS, c.textSecondary, centre, barTop + barH + 3, 'center')
  })
}

// ── The editor ──────────────────────────────────────────────────────────

function Displays(props: { editor: Editor }) {
  const e = props.editor
  const live = e.live
  const bypassed = e.bypassed
  const g = gateOf(e)
  const closeDb = g.thresholdDb - g.hysteresisDb
  const legend =
    g.hysteresisDb > 0.05
      ? `Open ${dbText(g.thresholdDb)} · Close ${dbText(closeDb)} dB`
      : `Threshold ${dbText(g.thresholdDb)} dB`
  const notice = bypassed ? 'Bypassed — WayGate passes audio through unchanged' : null
  return (
    <div className="gate-displays">
      <div className="gate-col" style={{ flexGrow: 4, ...minWidth(300) }}>
        <LiveCanvas
          draw={(ctx, w, h) => {
            ctx.translate(0, TAG_ROOM)
            paintGateHistory(ctx, w, h - TAG_ROOM, live, g, bypassed)
          }}
        >
          <DisplayTag>Gate history</DisplayTag>
          <DisplayTag right>10 s</DisplayTag>
          {notice && <span className="gate-notice">{notice}</span>}
        </LiveCanvas>
      </div>
      <div className="gate-col" style={{ flexGrow: 2, ...minWidth(230) }}>
        <LiveCanvas draw={(ctx, w, h) => paintResponse(ctx, w, h, g, live, bypassed)}>
          <DisplayTag>Response</DisplayTag>
          <DisplayTag right>{legend}</DisplayTag>
        </LiveCanvas>
      </div>
      <div className="gate-col gate-meters">
        <div className="gate-well">
          <LiveCanvas className="gate-bare" draw={(ctx, w, h) => paintGateMeters(ctx, w, h, live, g, bypassed)} />
        </div>
      </div>
    </div>
  )
}

/** kit's KitKnob, with the gate's readout. */
function GateKnob(props: { editor: Editor; id: string; size?: number }) {
  const { editor } = props
  const s = knobSpec(editor, props.id)
  if (!s) return null
  const [lo, hi] = knobRange(s)
  return (
    <div className="pe-knob">
      <Knob
        value={toKnob(s, editor.value(s.id))}
        min={lo}
        max={hi}
        defaultValue={toKnob(s, editor.defaultOf(s.id))}
        size={props.size ?? KNOB}
        label={`${s.label} (double-click: default)`}
        caption={s.label}
        format={(units) => readout(s, fromKnob(s, units))}
        onChange={(units) => editor.set(s.id, fromKnob(s, units))}
      />
    </div>
  )
}

function Controls(props: { editor: Editor }) {
  const e = props.editor
  const knob = (id: string, size = KNOB) => <GateKnob key={id} editor={e} id={id} size={size} />
  const knobCard = (title: string, knobs: ReactNode[]) => (
    <Card key={title} title={title} grow={knobs.length} style={minWidth(knobs.length * KNOB_PITCH + 18)}>
      <div className="pe-knobs">{knobs}</div>
    </Card>
  )
  const duck = Math.round(e.value('mode')) === 1
  const lookahead = e.value('lookaheadMs') > 0
  return (
    <Row>
      <Card title="MODE" grow={0} style={minWidth(170)}>
        <div className="gate-stack">
          <ParamChoice
            editor={e}
            id="mode"
            options={[
              [0, 'Gate'],
              [1, 'Duck'],
            ]}
            className="gate-track"
          />
          <span className="gate-hint">
            {duck
              ? 'Takes the range off while the key is above the threshold'
              : 'Opens above the threshold, takes the range off below'}
          </span>
        </div>
      </Card>
      {knobCard('LEVELS', [knob('thresholdDb', HERO_KNOB), knob('rangeDb', HERO_KNOB), knob('hysteresisDb')])}
      {knobCard('TIMING', [knob('attackMs'), knob('holdMs'), knob('releaseMs')])}
      <Card title="LOOKAHEAD" grow={1} style={minWidth(KNOB_PITCH + 18)}>
        <div className="gate-stack hair">
          {knob('lookaheadMs')}
          <span className="gate-hint">{lookahead ? 'Adds latency' : 'No latency'}</span>
        </div>
      </Card>
      <Card title="KEY FILTER" grow={2} style={minWidth(2 * KNOB_PITCH + 120)}>
        <div className="gate-cluster">
          {knob('keyHpfHz')}
          {knob('keyLpfHz')}
          <div className="gate-stack">
            <ParamCheck editor={e} id="keyListen" label="Key Listen" />
            <ParamCheck editor={e} id="stereoLink" label="Stereo Link" />
          </div>
        </div>
      </Card>
    </Row>
  )
}

function WayGateEditor(props: { editor: Editor }) {
  const { editor } = props
  const duck = Math.round(editor.value('mode')) === 1
  const ahead = editor.value('lookaheadMs')
  const subtitle = `${duck ? 'Ducker' : 'Noise gate'}${ahead > 0 ? ` · ${formatValue('ms', ahead)} lookahead` : ''}`
  return (
    // Key Listen is a listening aid, not a sound: a preset leaves it.
    <EditorShell editor={editor} title="WayGate" subtitle={subtitle} keep={['keyListen']}>
      <Displays editor={editor} />
      <Controls editor={editor} />
    </EditorShell>
  )
}

export const editors: Record<string, EditorComponent> = {
  waygate: WayGateEditor,
}
