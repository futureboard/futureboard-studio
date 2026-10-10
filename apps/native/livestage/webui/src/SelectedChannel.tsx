// The Selected Channel: one strip's whole section on one page, as a console's
// selected-channel screen — input and trim, high-pass, gate, the four-band EQ
// on its graph, the compressor on its transfer curve, delay, then the
// inserts, sends, DCA and mute-group assignment, colour, and the strip's
// fader, meters and solo safe. Opened from a strip's name (`#channel/…`).
//
// Every processing edit sends the whole section (`set_processing`), paced to
// at most 40 a second, and shows at once from a local draft, so a drag never
// waits on the server.

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { CSSProperties, PointerEvent as ReactPointerEvent, ReactNode } from 'react'
import {
  ArrowLeft,
  ArrowRight,
  BookOpen,
  ChevronLeft,
  ChevronRight,
  ClipboardCopy,
  ClipboardPaste,
  Copy,
  Grid3x3,
  Lock,
  LockOpen,
  Mic,
  ShieldCheck,
  SlidersVertical,
  Speaker,
} from 'lucide-react'
import { chipColorStyle, ColorSwatches, monitorPatched, soloHint, stripColorStyle } from './Console.tsx'
import { Fader, GateLight, GrBar, GrMeter, Knob, Latch, Meter } from './controls.tsx'
import { paintTransfer } from './editors/dynamics.tsx'
import type { Coeffs, FilterKind } from './editors/eq.tsx'
import { coefficients, responseDb } from './editors/eq.tsx'
import { Card, Choice, LiveCanvas, Row, Toggle } from './editors/kit.tsx'
import type { KnobSpec } from './editors/knobspec.ts'
import { bipolar, fromKnob, knobRange, readout, spec, toKnob } from './editors/knobspec.ts'
import type { Ctx } from './editors/paint.ts'
import { alpha, colors, freqAtFraction, freqFraction, line, rect } from './editors/paint.ts'
import { formatPan } from './faderLaw.ts'
import { BusSettings, MatrixSources, RoleBadge, patchedOutputs, sofStyle } from './Bus.tsx'
import type { InsertTarget } from './Mixer.tsx'
import { InputSelect, Inserts, Sends } from './Mixer.tsx'
import type { FoundStrip, SofTarget } from './routing.ts'
import { MATRIX_SOLO, findStrip as findStripRef, roleInfo } from './routing.ts'
import { consoleOrder } from './workstate.ts'
import {
  COMP_ATTACK_MS,
  COMP_KNEE_DB,
  COMP_MAKEUP_DB,
  COMP_RATIO,
  COMP_RELEASE_MS,
  COMP_THRESHOLD_DB,
  EQ_GAIN_DB,
  EQ_HZ,
  EQ_Q,
  GATE_ATTACK_MS,
  GATE_HOLD_MS,
  GATE_RANGE_DB,
  GATE_RELEASE_MS,
  GATE_THRESHOLD_DB,
  HPF_HZ,
  HPF_SLOPES,
  MAX_DELAY_MS,
  SOUND_M_PER_MS,
  clamp,
  cloneProcessing,
  compOutputDb,
  defaultProcessing,
  formatHz,
} from './processing.ts'
import type {
  ChannelStrip,
  EqBand,
  EqKind,
  Processing,
  ProcessingOrder,
  Session,
  StripCore,
  StripRef,
} from './protocol.ts'
import { stripKey } from './protocol.ts'
import { act, actLatest, actPaced, useStore } from './store.ts'
import { MenuButton, MenuItem, copyDefault, pasteClip, pasteLabel } from './Workflow.tsx'
import { openDialog, targetsFor, useWork } from './workstate.ts'

// ── Finding the strip ───────────────────────────────────────────────────

interface Found extends FoundStrip {
  /** "3", a role, a matrix or the master: the badge. */
  badge: ReactNode
}

function findStrip(session: Session, strip: StripRef): Found | null {
  const found = findStripRef(session, strip)
  if (!found) return null
  let badge: ReactNode = <Speaker size={13} />
  if (found.channel) badge = session.channels.indexOf(found.channel) + 1
  else if (found.bus) badge = <RoleBadge role={found.bus.role} />
  else if (found.matrix) badge = <Grid3x3 size={13} />
  return { ...found, badge }
}

/** Every strip in console order: channels, buses, matrices, the master. */
const allStrips = consoleOrder

// ── The draft ───────────────────────────────────────────────────────────

/** How long an edit is shown from the draft after the last change, before
 *  the session's value (the server's echo) takes over again. */
const DRAFT_MS = 800

type EditProcessing = (change: (p: Processing) => void) => void

/** The section as the user is moving it: edits apply to the draft at once
 *  and go to the server paced; the session's value returns once they stop. */
function useProcessingDraft(strip: StripRef, server: Processing): [Processing, EditProcessing] {
  const [draft, setDraft] = useState<Processing | null>(null)
  const draftRef = useRef<Processing | null>(null)
  const serverRef = useRef(server)
  serverRef.current = server
  const timer = useRef<number | undefined>(undefined)
  useEffect(() => () => window.clearTimeout(timer.current), [])
  const key = stripKey(strip)
  const edit = useCallback<EditProcessing>(
    (change) => {
      const next = cloneProcessing(draftRef.current ?? serverRef.current)
      change(next)
      draftRef.current = next
      setDraft(next)
      actPaced(`processing:${key}`, { cmd: 'set_processing', strip, processing: next })
      window.clearTimeout(timer.current)
      timer.current = window.setTimeout(() => {
        draftRef.current = null
        setDraft(null)
      }, DRAFT_MS)
    },
    // The same strip as long as the key is.
    [key],
  )
  return [draft ?? server, edit]
}

// ── Knobs ───────────────────────────────────────────────────────────────

const KNOB_SIZE = 34
const SMALL_KNOB = 30

const S = {
  hpfHz: spec('hpf.hz', 'Freq', HPF_HZ[0], HPF_HZ[1], 'log', 'hz'),
  gateThreshold: spec('gate.threshold', 'Threshold', GATE_THRESHOLD_DB[0], GATE_THRESHOLD_DB[1], 'linear', 'db'),
  gateRange: spec('gate.range', 'Range', GATE_RANGE_DB[0], GATE_RANGE_DB[1], 'linear', 'db'),
  gateAttack: spec('gate.attack', 'Attack', GATE_ATTACK_MS[0], GATE_ATTACK_MS[1], 'log', 'ms'),
  gateHold: spec('gate.hold', 'Hold', GATE_HOLD_MS[0], GATE_HOLD_MS[1], 'square', 'ms'),
  gateRelease: spec('gate.release', 'Release', GATE_RELEASE_MS[0], GATE_RELEASE_MS[1], 'log', 'ms'),
  eqHz: spec('eq.hz', 'Freq', EQ_HZ[0], EQ_HZ[1], 'log', 'hz'),
  eqGain: bipolar(spec('eq.gain', 'Gain', EQ_GAIN_DB[0], EQ_GAIN_DB[1], 'linear', 'db')),
  eqQ: spec('eq.q', 'Q', EQ_Q[0], EQ_Q[1], 'log', 'times'),
  compThreshold: spec('comp.threshold', 'Threshold', COMP_THRESHOLD_DB[0], COMP_THRESHOLD_DB[1], 'linear', 'db'),
  compRatio: spec('comp.ratio', 'Ratio', COMP_RATIO[0], COMP_RATIO[1], 'log', 'ratio'),
  compAttack: spec('comp.attack', 'Attack', COMP_ATTACK_MS[0], COMP_ATTACK_MS[1], 'log', 'ms'),
  compRelease: spec('comp.release', 'Release', COMP_RELEASE_MS[0], COMP_RELEASE_MS[1], 'log', 'ms'),
  compKnee: spec('comp.knee', 'Knee', COMP_KNEE_DB[0], COMP_KNEE_DB[1], 'linear', 'db'),
  compMakeup: spec('comp.makeup', 'Makeup', COMP_MAKEUP_DB[0], COMP_MAKEUP_DB[1], 'linear', 'db'),
  delayMs: spec('delay.ms', 'Delay', 0, MAX_DELAY_MS, 'square', 'ms'),
}

/** A knob over a processing value: `spec` maps and reads it, double-click
 *  returns it to `defaultValue`. */
function PKnob(props: {
  spec: KnobSpec
  value: number
  defaultValue: number
  onChange: (value: number) => void
  size?: number
  format?: (value: number) => string
}) {
  const { spec: s } = props
  const [lo, hi] = knobRange(s)
  const text = (v: number) => (props.format ? props.format(v) : readout(s, v))
  return (
    <div className="pe-knob">
      <Knob
        value={toKnob(s, props.value)}
        min={lo}
        max={hi}
        defaultValue={toKnob(s, props.defaultValue)}
        bipolar={s.bipolar}
        size={props.size ?? KNOB_SIZE}
        label={`${s.label} (double-click: default)`}
        caption={s.label}
        format={(units) => text(fromKnob(s, units))}
        onChange={(units) => props.onChange(fromKnob(s, units))}
      />
    </div>
  )
}

const formatQ = (q: number) => q.toFixed(q < 1 ? 2 : 1)

// ── The view ────────────────────────────────────────────────────────────

export function SelectedChannel(props: {
  session: Session
  strip: StripRef
  inputs: number
  sampleRate: number
  onClose: () => void
  onSelect: (strip: StripRef) => void
  onOpenInsert: (target: InsertTarget) => void
  onAddEffect: (strip: StripRef) => void
  onSof: (target: SofTarget) => void
}) {
  const { session, strip, onClose } = props
  const found = findStrip(session, strip)
  // Removed (here or elsewhere): back to the mixer.
  useEffect(() => {
    if (!found) onClose()
  }, [found, onClose])
  if (!found) return null
  return <ChannelPage key={stripKey(strip)} {...props} found={found} />
}

function ChannelPage(props: {
  session: Session
  strip: StripRef
  found: Found
  inputs: number
  sampleRate: number
  onClose: () => void
  onSelect: (strip: StripRef) => void
  onOpenInsert: (target: InsertTarget) => void
  onAddEffect: (strip: StripRef) => void
  onSof: (target: SofTarget) => void
}) {
  const { session, strip, found } = props
  const { core } = found
  const key = stripKey(strip)
  const insertStates = useStore((s) => s.insertStates)
  const [p, edit] = useProcessingDraft(strip, core.processing)
  const [band, setBand] = useState<number | null>(null)

  const strips = allStrips(session)
  const at = strips.findIndex((s) => stripKey(s) === key)
  const prev = at > 0 ? strips[at - 1] : null
  const next = at >= 0 && at < strips.length - 1 ? strips[at + 1] : null
  const movable = strip.kind !== 'master'
  const siblings: { id: number }[] =
    strip.kind === 'channel'
      ? session.channels
      : strip.kind === 'bus'
        ? session.buses
        : strip.kind === 'matrix'
          ? session.matrices
          : []
  const position = movable ? siblings.findIndex((s) => s.id === strip.id) : -1
  const move = (to: number) => {
    if (strip.kind === 'channel') act({ cmd: 'move_channel', channel: strip.id, index: to })
    if (strip.kind === 'bus') act({ cmd: 'move_bus', bus: strip.id, index: to })
    if (strip.kind === 'matrix') act({ cmd: 'move_matrix', matrix: strip.id, index: to })
  }

  let kindLabel = 'Master'
  if (found.channel) kindLabel = `Channel ${position + 1} · ${inputText(found.channel)}`
  else if (found.bus) kindLabel = `${roleInfo(found.bus.role).label} bus · ${found.bus.stereo ? 'stereo' : 'mono'}`
  else if (found.matrix) kindLabel = `Matrix · ${found.matrix.stereo ? 'stereo' : 'mono'}`

  const eqCard = <EqCard key="eq" p={p} edit={edit} sampleRate={props.sampleRate} band={band} onBand={setBand} />
  const compCard = (
    <Row key="comp">
      <CompCard p={p} edit={edit} strip={key} />
    </Row>
  )

  return (
    <div className={`cv${core.color !== null ? ' colored' : ''}`} style={stripColorStyle(core.color)}>
      <header className="cv-head">
        <button type="button" className="button ghost cv-back" onClick={props.onClose} title="Back to the mixer">
          <SlidersVertical size={15} /> <span>Mixer</span>
        </button>
        <div className="cv-nav">
          <button
            type="button"
            className="icon-button large"
            disabled={!prev}
            title="Previous strip"
            aria-label="Previous strip"
            onClick={() => prev && props.onSelect(prev)}
          >
            <ChevronLeft size={16} />
          </button>
          <button
            type="button"
            className="icon-button large"
            disabled={!next}
            title="Next strip"
            aria-label="Next strip"
            onClick={() => next && props.onSelect(next)}
          >
            <ChevronRight size={16} />
          </button>
        </div>
        <span className="cv-badge">{found.badge}</span>
        <div className="cv-title">
          {movable ? <NameField strip={strip} name={found.name} /> : <strong className="cv-name-static">Master</strong>}
          <span className="cv-kind">{kindLabel}</span>
        </div>
        {movable && (
          <div className="cv-move">
            <button
              type="button"
              className="icon-button large"
              disabled={position <= 0}
              title="Move one place earlier"
              aria-label="Move earlier"
              onClick={() => move(position - 1)}
            >
              <ArrowLeft size={15} />
            </button>
            <button
              type="button"
              className="icon-button large"
              disabled={position < 0 || position >= siblings.length - 1}
              title="Move one place later"
              aria-label="Move later"
              onClick={() => move(position + 1)}
            >
              <ArrowRight size={15} />
            </button>
          </div>
        )}
        <ChannelTools session={session} strip={strip} />
      </header>

      <Flow p={p} edit={edit} inserts={core.inserts.length} />

      <div className="cv-body">
        <div className="cv-main">
          <Row>
            {found.channel && <InputCard channel={found.channel} inputs={props.inputs} />}
            <HpfCard p={p} edit={edit} />
            <GateCard p={p} edit={edit} strip={key} />
          </Row>
          {p.order === 'eq_then_comp' ? [eqCard, compCard] : [compCard, eqCard]}
          <Row>
            <DelayCard p={p} edit={edit} />
            <Card title="Inserts" grow={2} minWidth={200} className="cv-rack">
              <Inserts
                inserts={core.inserts}
                states={insertStates}
                onOpen={(insert) => props.onOpenInsert({ strip, insert })}
                onAdd={() => props.onAddEffect(strip)}
              />
            </Card>
            {found.channel && (
              <Card title="Sends" grow={3} minWidth={260} className="cv-rack">
                <Sends channel={found.channel} buses={session.buses} wide />
              </Card>
            )}
            {found.bus && (
              <Card title="Bus" grow={2} minWidth={240}>
                <BusSettings bus={found.bus} onSof={props.onSof} />
              </Card>
            )}
          </Row>
          {found.matrix && (
            <Row>
              <Card
                title="Sources"
                grow={1}
                minWidth={260}
                aside={
                  <span className="cv-section-aside">
                    <Choice
                      value={found.matrix.stereo ? 'stereo' : 'mono'}
                      options={[
                        ['stereo', 'Stereo'],
                        ['mono', 'Mono'],
                      ]}
                      onChange={(w) =>
                        found.matrix &&
                        act({ cmd: 'set_matrix_stereo', matrix: found.matrix.id, stereo: w === 'stereo' })
                      }
                    />
                    <button
                      type="button"
                      className="button small sof-enter"
                      style={sofStyle(found.matrix.color)}
                      title="Show the master's and every bus's level into this matrix on the faders"
                      onClick={() => found.matrix && props.onSof({ kind: 'matrix', id: found.matrix.id })}
                    >
                      <SlidersVertical size={13} /> On faders
                    </button>
                  </span>
                }
              >
                <MatrixSources session={session} matrix={found.matrix} />
                <span className="pe-note">
                  The master and buses after their faders, summed at these levels, then this strip's processing (its
                  delay lines a zone up, up to 1 s), inserts and fader, to {patchedOutputs(session, { kind: 'matrix', id: found.matrix.id }) || 'no output yet (Patch → Outputs)'}.
                </span>
              </Card>
            </Row>
          )}
          <Row>
            {strip.kind !== 'master' ? (
              <AssignCard session={session} strip={strip} core={core} />
            ) : (
              <Card title="DCA and mute groups" grow={3} minWidth={240}>
                <span className="pe-note">The master follows no DCA or mute group.</span>
              </Card>
            )}
            <Card title="Colour" grow={2} minWidth={220}>
              <ColorSwatches value={core.color} onChange={(color) => act({ cmd: 'set_strip_color', strip, color })} />
            </Card>
          </Row>
        </div>
        <aside className="cv-side">
          <SideCard
            strip={strip}
            core={core}
            meterTap={found.channel !== null}
            soloHint={soloHint(session)}
            soloSilent={session.monitor.solo_mode !== 'sip' && !monitorPatched(session)}
          />
        </aside>
      </div>
    </div>
  )
}

function inputText(channel: ChannelStrip): string {
  const { left, right } = channel.input
  if (left === null) return 'no input'
  return right === null ? `In ${left + 1}` : `In ${left + 1}/${right + 1}`
}

function NameField(props: { strip: StripRef; name: string }) {
  return (
    <input
      key={props.name}
      className="cv-name"
      defaultValue={props.name}
      aria-label="Name"
      title="Rename"
      onBlur={(e) => {
        const name = e.currentTarget.value.trim()
        if (name && name !== props.name) act({ cmd: 'rename_strip', strip: props.strip, name })
        else e.currentTarget.value = props.name
      }}
      onKeyDown={(e) => {
        if (e.key === 'Enter') e.currentTarget.blur()
        if (e.key === 'Escape') {
          e.currentTarget.value = props.name
          e.currentTarget.blur()
        }
      }}
    />
  )
}

/** The signal path, left to right, each stage lit while it is in, with the
 *  EQ/compressor order switch where the two meet. */
function Flow(props: { p: Processing; edit: EditProcessing; inserts: number }) {
  const { p } = props
  const stage = (label: string, on: boolean, title: string) => (
    <span key={label} className={`flow-stage${on ? ' on' : ''}`} title={title}>
      {label}
    </span>
  )
  const eq = stage('EQ', p.eq.on, p.eq.on ? 'EQ in' : 'EQ out')
  const comp = stage('Comp', p.comp.on, p.comp.on ? 'Compressor in' : 'Compressor out')
  const arrow = (k: string) => (
    <span key={k} className="flow-arrow" aria-hidden>
      →
    </span>
  )
  return (
    <div className="cv-flow" aria-label="Signal path">
      <div className="flow-line">
        {stage('HPF', p.hpf.on, p.hpf.on ? 'High-pass in' : 'High-pass out')}
        {arrow('a1')}
        {stage('Gate', p.gate.on, p.gate.on ? 'Gate in' : 'Gate out')}
        {arrow('a2')}
        {p.order === 'eq_then_comp' ? [eq, arrow('a3'), comp] : [comp, arrow('a3'), eq]}
        {arrow('a4')}
        {stage('Delay', p.delay.on && p.delay.ms > 0, p.delay.on ? `Delay ${p.delay.ms.toFixed(1)} ms` : 'Delay out')}
        {arrow('a5')}
        {stage(`Inserts${props.inserts > 0 ? ` ${props.inserts}` : ''}`, props.inserts > 0, `${props.inserts} insert(s)`)}
        {arrow('a6')}
        {stage('Fader', true, 'Fader and pan')}
      </div>
      <Choice<ProcessingOrder>
        value={p.order}
        options={[
          ['eq_then_comp', 'EQ → Comp'],
          ['comp_then_eq', 'Comp → EQ'],
        ]}
        title="Processing order"
        className="flow-order"
        onChange={(order) => props.edit((n) => void (n.order = order))}
      />
    </div>
  )
}

/** A section's card: its name, its In switch, and the gate's open light or
 *  anything else in the head. */
function Section(props: {
  title: string
  on: boolean
  onToggle: () => void
  grow?: number
  minWidth?: number
  aside?: ReactNode
  className?: string
  children: ReactNode
}) {
  return (
    <Card
      title={props.title}
      grow={props.grow}
      minWidth={props.minWidth}
      className={`cv-section${props.on ? '' : ' out'}${props.className ? ` ${props.className}` : ''}`}
      aside={
        <span className="cv-section-aside">
          {props.aside}
          <Toggle on={props.on} onClick={props.onToggle} title={props.on ? 'In: click to take it out' : 'Out: click to put it in'}>
            {props.on ? 'In' : 'Out'}
          </Toggle>
        </span>
      }
    >
      {props.children}
    </Card>
  )
}

const D = defaultProcessing()

function InputCard(props: { channel: ChannelStrip; inputs: number }) {
  const { channel } = props
  return (
    <Card title="Input" grow={2} minWidth={190}>
      <InputSelect channel={channel} inputs={props.inputs} />
      <div className="cv-knobs">
        <div className="pe-knob">
          <Knob
            value={channel.trim_db}
            min={-24}
            max={24}
            defaultValue={0}
            bipolar
            size={KNOB_SIZE}
            label="Trim (double-click: 0 dB)"
            caption="Trim"
            format={(v) => `${v > 0 ? '+' : ''}${v.toFixed(1)} dB`}
            onChange={(db) => actLatest(`trim:${channel.id}`, { cmd: 'set_trim', channel: channel.id, db })}
          />
        </div>
        <div className="cv-latch-cell">
          <Latch
            kind="plain"
            on={channel.phase_invert}
            title="Polarity invert"
            onClick={() => act({ cmd: 'set_phase_invert', channel: channel.id, invert: !channel.phase_invert })}
          >
            Ø
          </Latch>
          <span className="knob-caption">Polarity</span>
        </div>
      </div>
    </Card>
  )
}

function HpfCard(props: { p: Processing; edit: EditProcessing }) {
  const { p, edit } = props
  return (
    <Section title="High-pass" on={p.hpf.on} onToggle={() => edit((n) => void (n.hpf.on = !n.hpf.on))} grow={2} minWidth={190}>
      <div className="cv-knobs">
        <PKnob
          spec={S.hpfHz}
          value={p.hpf.hz}
          defaultValue={D.hpf.hz}
          onChange={(v) => edit((n) => void (n.hpf.hz = clamp(v, HPF_HZ)))}
        />
        <div className="cv-choice-cell">
          <Choice<number>
            value={p.hpf.slope_db}
            options={HPF_SLOPES.map((s): [number, string] => [s, `${s}`])}
            title="Slope, dB per octave"
            onChange={(slope) => edit((n) => void (n.hpf.slope_db = slope))}
          />
          <span className="knob-caption">dB/oct</span>
        </div>
      </div>
    </Section>
  )
}

function GateCard(props: { p: Processing; edit: EditProcessing; strip: string }) {
  const { p, edit } = props
  const g = p.gate
  return (
    <Section
      title="Gate"
      on={g.on}
      onToggle={() => edit((n) => void (n.gate.on = !n.gate.on))}
      grow={4}
      minWidth={300}
      aside={<GateLight strip={props.strip} on={g.on} />}
    >
      <div className="cv-knobs">
        <PKnob
          spec={S.gateThreshold}
          value={g.threshold_db}
          defaultValue={D.gate.threshold_db}
          onChange={(v) => edit((n) => void (n.gate.threshold_db = clamp(v, GATE_THRESHOLD_DB)))}
        />
        <PKnob
          spec={S.gateRange}
          value={g.range_db}
          defaultValue={D.gate.range_db}
          onChange={(v) => edit((n) => void (n.gate.range_db = clamp(v, GATE_RANGE_DB)))}
        />
        <PKnob
          spec={S.gateAttack}
          value={g.attack_ms}
          defaultValue={D.gate.attack_ms}
          size={SMALL_KNOB}
          onChange={(v) => edit((n) => void (n.gate.attack_ms = clamp(v, GATE_ATTACK_MS)))}
        />
        <PKnob
          spec={S.gateHold}
          value={g.hold_ms}
          defaultValue={D.gate.hold_ms}
          size={SMALL_KNOB}
          onChange={(v) => edit((n) => void (n.gate.hold_ms = clamp(v, GATE_HOLD_MS)))}
        />
        <PKnob
          spec={S.gateRelease}
          value={g.release_ms}
          defaultValue={D.gate.release_ms}
          size={SMALL_KNOB}
          onChange={(v) => edit((n) => void (n.gate.release_ms = clamp(v, GATE_RELEASE_MS)))}
        />
      </div>
      <div className="cv-gr-row">
        <span className="knob-caption">Reduction</span>
        <GrBar strip={props.strip} which="gate" on={g.on} />
      </div>
    </Section>
  )
}

// ── EQ ──────────────────────────────────────────────────────────────────

const CURVE_POINTS = 256
const GRAPH_RANGE_DB = 18
const NODE_HIT_PX = 12
const NODE_HIT_TOUCH_PX = 22
/** Drag travel with Shift held. */
const FINE = 0.25
const DOUBLE_MS = 400
const BAND_NAMES = ['Low', 'Low mid', 'High mid', 'High']
const KINDS: [EqKind, string][] = [
  ['low_shelf', 'LS'],
  ['bell', 'Bell'],
  ['high_shelf', 'HS'],
]
const KIND_TITLE: Record<EqKind, string> = { low_shelf: 'Low shelf', bell: 'Bell', high_shelf: 'High shelf' }
const FILTER: Record<EqKind, FilterKind> = { low_shelf: 'lowshelf', bell: 'bell', high_shelf: 'highshelf' }
const FREQ_GRID = [
  20, 30, 40, 50, 60, 70, 80, 90, 100, 200, 300, 400, 500, 600, 700, 800, 900, 1000, 2000, 3000, 4000, 5000, 6000,
  7000, 8000, 9000, 10000, 20000,
]
const FREQ_LABELS: [number, string][] = [
  [30, '30'],
  [100, '100'],
  [300, '300'],
  [1000, '1k'],
  [3000, '3k'],
  [10000, '10k'],
]
const DB_LINES = [18, 12, 6, 0, -6, -12, -18]

const dbFraction = (db: number) => 0.5 - db / (2 * GRAPH_RANGE_DB)
const dbAtFraction = (f: number) => (0.5 - f) * 2 * GRAPH_RANGE_DB
const sampleHz = (i: number) => freqAtFraction(i / (CURVE_POINTS - 1))
const bandColor = (i: number) => colors().bands[i % colors().bands.length]

/** A Butterworth high-pass of `slope` dB/oct at `hz`: the analogue response
 *  the section's cascade follows. */
function hpfDb(hz: number, cutoff: number, slope: number): number {
  const order = Math.max(1, Math.round(slope / 6))
  return -10 * Math.log10(1 + Math.pow(cutoff / Math.max(1, hz), 2 * order))
}

function bandSection(b: EqBand, sampleRate: number): Coeffs | null {
  return coefficients(FILTER[b.kind], b.hz, b.gain_db, b.q, sampleRate)
}

interface EqModel {
  bands: number[][]
  hpf: number[] | null
  total: number[]
}

function eqModel(p: Processing, sampleRate: number): EqModel {
  const bands = p.eq.bands.map((b) => {
    const c = bandSection(b, sampleRate)
    return Array.from({ length: CURVE_POINTS }, (_, i) => (c ? responseDb(c, sampleHz(i), sampleRate) : 0))
  })
  const hpf = p.hpf.on ? Array.from({ length: CURVE_POINTS }, (_, i) => hpfDb(sampleHz(i), p.hpf.hz, p.hpf.slope_db)) : null
  const total = Array.from({ length: CURVE_POINTS }, (_, i) => {
    let db = hpf ? hpf[i] : 0
    if (p.eq.on) for (const curve of bands) db += curve[i]
    return db
  })
  return { bands, hpf, total }
}

type Node = number | 'hpf'

/** Where a node sits, as fractions across and down the plot. */
function nodePlace(p: Processing, node: Node): [number, number] {
  if (node === 'hpf') return [freqFraction(p.hpf.hz), dbFraction(-3)]
  const b = p.eq.bands[node]
  return [freqFraction(b.hz), Math.min(1, Math.max(0, dbFraction(b.gain_db)))]
}

function paintEq(ctx: Ctx, w: number, h: number, m: EqModel, p: Processing, selected: number | null) {
  const c = colors()
  for (const hz of FREQ_GRID) {
    const major = FREQ_LABELS.some(([at]) => at === hz)
    rect(ctx, Math.round(freqFraction(hz) * w), 0, 1, h, alpha(c.text, major ? 0.09 : 0.045))
  }
  for (const db of DB_LINES) rect(ctx, 0, Math.round(dbFraction(db) * h), w, 1, alpha(c.text, db === 0 ? 0.18 : 0.08))
  const toPoints = (curve: number[]) =>
    curve.map((db, i): [number, number] => [
      (i / (CURVE_POINTS - 1)) * w,
      Math.min(h, Math.max(0, dbFraction(Math.max(-GRAPH_RANGE_DB * 1.4, Math.min(GRAPH_RANGE_DB * 1.4, db))) * h)),
    ])
  const zeroY = dbFraction(0) * h
  const eqDim = p.eq.on ? 1 : 0.35
  m.bands.forEach((curve, i) => {
    const points = toPoints(curve)
    const color = bandColor(i)
    if (selected === i) {
      fillToZero(ctx, points, zeroY, alpha(color, 0.14 * eqDim))
      line(ctx, points, 1.4, alpha(color, 0.9 * eqDim))
    } else {
      line(ctx, points, 1, alpha(color, 0.38 * eqDim))
    }
  })
  if (m.hpf) line(ctx, toPoints(m.hpf), 1, alpha(c.textSecondary, 0.5))
  const total = toPoints(m.total)
  fillToZero(ctx, total, zeroY, alpha(c.accent, 0.12))
  line(ctx, total, 2, c.accent)
}

function fillToZero(ctx: Ctx, points: [number, number][], zeroY: number, color: string) {
  ctx.beginPath()
  ctx.moveTo(points[0][0], zeroY)
  for (const [x, y] of points) ctx.lineTo(x, y)
  ctx.lineTo(points[points.length - 1][0], zeroY)
  ctx.closePath()
  ctx.fillStyle = color
  ctx.fill()
}

function EqCard(props: {
  p: Processing
  edit: EditProcessing
  sampleRate: number
  band: number | null
  onBand: (band: number | null) => void
}) {
  const { p, edit } = props
  const flat = p.eq.bands.every((b) => Math.abs(b.gain_db) < 0.05)
  return (
    <Section
      title="EQ"
      on={p.eq.on}
      onToggle={() => edit((n) => void (n.eq.on = !n.eq.on))}
      className="cv-eq"
      aside={
        <button
          type="button"
          className="pe-toggle"
          disabled={flat}
          title="Every band's gain to 0 dB"
          onClick={() =>
            edit((n) => {
              for (const b of n.eq.bands) b.gain_db = 0
            })
          }
        >
          Flat
        </button>
      }
    >
      <EqGraph p={p} edit={edit} sampleRate={props.sampleRate} selected={props.band} onSelect={props.onBand} />
      <div className="cv-bands">
        {p.eq.bands.map((b, i) => (
          <div
            key={i}
            className={`cv-band${props.band === i ? ' selected' : ''}`}
            style={{ '--band-color': `var(--eq-band-${i + 1})` } as CSSProperties}
            onPointerDown={() => props.onBand(i)}
          >
            <div className="cv-band-head">
              <span className="cv-band-num">{i + 1}</span>
              <span className="cv-band-name">{BAND_NAMES[i]}</span>
            </div>
            <Choice<EqKind>
              value={b.kind}
              options={KINDS}
              className="cv-band-kind"
              title={KIND_TITLE[b.kind]}
              onChange={(kind) => edit((n) => void (n.eq.bands[i].kind = kind))}
            />
            <div className="cv-band-knobs">
              <PKnob
                spec={S.eqHz}
                value={b.hz}
                defaultValue={D.eq.bands[i].hz}
                size={SMALL_KNOB}
                onChange={(v) => edit((n) => void (n.eq.bands[i].hz = clamp(v, EQ_HZ)))}
              />
              <PKnob
                spec={S.eqGain}
                value={b.gain_db}
                defaultValue={0}
                size={SMALL_KNOB}
                onChange={(v) => edit((n) => void (n.eq.bands[i].gain_db = clamp(v, EQ_GAIN_DB)))}
              />
              <PKnob
                spec={S.eqQ}
                value={b.q}
                defaultValue={D.eq.bands[i].q}
                size={SMALL_KNOB}
                format={(q) => formatQ(q)}
                onChange={(v) => edit((n) => void (n.eq.bands[i].q = clamp(v, EQ_Q)))}
              />
            </div>
          </div>
        ))}
      </div>
    </Section>
  )
}

/** The EQ's curve, with a node per band (drag: frequency and gain; wheel:
 *  Q; double-click: gain to 0) and the high-pass's node (drag: frequency). */
function EqGraph(props: {
  p: Processing
  edit: EditProcessing
  sampleRate: number
  selected: number | null
  onSelect: (band: number | null) => void
}) {
  const { p, edit } = props
  const sampleRate = props.sampleRate > 0 ? props.sampleRate : 48000
  const shape = JSON.stringify([p.eq, p.hpf])
  const model = useMemo(() => eqModel(p, sampleRate), [shape, sampleRate])
  const latest = useRef({ p, model, selected: props.selected })
  latest.current = { p, model, selected: props.selected }
  const plot = useRef<HTMLDivElement>(null)
  const drag = useRef<{ node: Node; origin: [number, number]; start: [number, number] } | null>(null)
  const lastDown = useRef<{ node: Node; at: number } | null>(null)
  const [active, setActive] = useState<Node | null>(null)

  const nodes: Node[] = [0, 1, 2, 3, ...(p.hpf.on ? ['hpf' as const] : [])]

  const nodeAt = (x: number, y: number, w: number, h: number, radius: number): Node | null => {
    let best: [Node, number] | null = null
    for (const node of nodes) {
      const [fx, fy] = nodePlace(p, node)
      let distance = Math.hypot(fx * w - x, fy * h - y)
      if (distance > radius) continue
      if (node === props.selected) distance -= 0.5
      if (!best || distance < best[1]) best = [node, distance]
    }
    return best ? best[0] : null
  }

  const onDown = (x: number, y: number, e: ReactPointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return
    const { clientWidth: w, clientHeight: h } = e.currentTarget
    const node = nodeAt(x, y, w, h, e.pointerType === 'touch' ? NODE_HIT_TOUCH_PX : NODE_HIT_PX)
    if (node === null) return
    const now = performance.now()
    const double = lastDown.current !== null && lastDown.current.node === node && now - lastDown.current.at < DOUBLE_MS
    lastDown.current = double ? null : { node, at: now }
    if (typeof node === 'number') props.onSelect(node)
    if (double && typeof node === 'number') {
      edit((n) => void (n.eq.bands[node].gain_db = 0))
      drag.current = null
      return
    }
    drag.current = { node, origin: [x, y], start: nodePlace(p, node) }
    setActive(node)
  }

  const onMove = (x: number, y: number, e: ReactPointerEvent<HTMLDivElement>) => {
    const d = drag.current
    if (!d) return
    if (e.buttons === 0) {
      drag.current = null
      setActive(null)
      return
    }
    const { clientWidth: w, clientHeight: h } = e.currentTarget
    const scale = e.shiftKey ? FINE : 1
    const fx = d.start[0] + ((x - d.origin[0]) / Math.max(1, w)) * scale
    const fy = d.start[1] + ((y - d.origin[1]) / Math.max(1, h)) * scale
    const hz = freqAtFraction(fx)
    if (d.node === 'hpf') {
      edit((n) => void (n.hpf.hz = Math.round(clamp(hz, HPF_HZ))))
      return
    }
    const band = d.node
    const gain = Math.round(clamp(dbAtFraction(fy), EQ_GAIN_DB) * 10) / 10
    edit((n) => {
      n.eq.bands[band].hz = clamp(hz >= 1000 ? Math.round(hz / 10) * 10 : Math.round(hz), EQ_HZ)
      // Alt keeps the gain where it is.
      if (!e.altKey) n.eq.bands[band].gain_db = gain
    })
  }

  const onUp = () => {
    drag.current = null
    setActive(null)
  }

  // A wheel that may cancel the page's scroll needs a non-passive listener.
  const wheel = useRef<(e: WheelEvent) => void>(() => {})
  wheel.current = (e: WheelEvent) => {
    const box = plot.current
    if (!box || e.deltaY === 0) return
    const r = box.getBoundingClientRect()
    const hit = nodeAt(e.clientX - r.left, e.clientY - r.top, box.clientWidth, box.clientHeight, NODE_HIT_TOUCH_PX)
    const band = typeof hit === 'number' ? hit : latest.current.selected
    if (band === null) return
    e.preventDefault()
    const notch = e.shiftKey ? 1.03 : 1.15
    const factor = e.deltaY < 0 ? notch : 1 / notch
    props.onSelect(band)
    edit((n) => void (n.eq.bands[band].q = Math.round(clamp(n.eq.bands[band].q * factor, EQ_Q) * 100) / 100))
  }
  useEffect(() => {
    const box = plot.current
    if (!box) return
    const listener = (e: WheelEvent) => wheel.current(e)
    box.addEventListener('wheel', listener, { passive: false })
    return () => box.removeEventListener('wheel', listener)
  }, [])

  let readoutText: string | null = null
  if (active === 'hpf') readoutText = `High-pass · ${formatHz(p.hpf.hz)} Hz · ${p.hpf.slope_db} dB/oct`
  else if (active !== null) {
    const b = p.eq.bands[active]
    readoutText = `${active + 1} ${KIND_TITLE[b.kind]} · ${formatHz(b.hz)} Hz · ${b.gain_db > 0 ? '+' : ''}${b.gain_db.toFixed(1)} dB · Q ${formatQ(b.q)}`
  }

  return (
    <div className="cv-graph">
      <div className="cv-plot-row">
        <div className="cv-db">
          {DB_LINES.filter((db) => Math.abs(db) < GRAPH_RANGE_DB).map((db) => (
            <span key={db} className={db === 0 ? 'zero' : ''} style={{ top: `${dbFraction(db) * 100}%` }}>
              {db > 0 ? `+${db}` : db}
            </span>
          ))}
        </div>
        <div ref={plot} className={`cv-plot${active !== null ? ' dragging' : ''}`} onContextMenu={(e) => e.preventDefault()}>
          <LiveCanvas
            draw={(ctx, w, h) => paintEq(ctx, w, h, latest.current.model, latest.current.p, latest.current.selected)}
            onPointerDown={onDown}
            onPointerMove={onMove}
            onPointerUp={onUp}
          >
            {nodes.map((node) => {
              const [fx, fy] = nodePlace(p, node)
              const isHpf = node === 'hpf'
              const selected = node === props.selected || node === active
              const color = isHpf ? 'var(--text-secondary)' : `var(--eq-band-${node + 1})`
              const lit = isHpf || p.eq.on
              return (
                <span
                  key={String(node)}
                  className={`cv-node${lit ? '' : ' off'}${selected ? ' selected' : ''}`}
                  style={{
                    left: `${fx * 100}%`,
                    top: `${fy * 100}%`,
                    background: lit ? color : 'var(--surface-canvas)',
                    color: lit ? 'var(--surface-canvas)' : color,
                    borderColor: selected ? 'var(--text-primary)' : color,
                  }}
                >
                  {isHpf ? 'H' : node + 1}
                </span>
              )
            })}
            {readoutText && <span className="cv-readout">{readoutText}</span>}
            {!p.eq.on && <span className="cv-graph-note">EQ out</span>}
          </LiveCanvas>
        </div>
      </div>
      <div className="cv-freq">
        {FREQ_LABELS.map(([hz, text]) => (
          <span key={hz} style={{ left: `${freqFraction(hz) * 100}%` }}>
            {text}
          </span>
        ))}
      </div>
    </div>
  )
}

// ── Compressor ──────────────────────────────────────────────────────────

const TRANSFER_RANGE_DB = -60

let warningInk: string | null = null
function warningColor(): string {
  warningInk ??= getComputedStyle(document.documentElement).getPropertyValue('--warning').trim()
  return warningInk
}

function CompCard(props: { p: Processing; edit: EditProcessing; strip: string }) {
  const { p, edit } = props
  const comp = p.comp
  const latest = useRef(comp)
  latest.current = comp
  return (
    <Section
      title="Compressor"
      on={comp.on}
      onToggle={() => edit((n) => void (n.comp.on = !n.comp.on))}
      grow={1}
      minWidth={280}
      className="cv-comp"
    >
      <div className="cv-comp-body">
        <div className="cv-transfer-wrap">
          <LiveCanvas
            className="cv-transfer"
            draw={(ctx, w, h) => {
              const c = latest.current
              paintTransfer(
                ctx,
                w,
                h,
                (input) => compOutputDb(c, input),
                TRANSFER_RANGE_DB,
                [{ db: c.threshold_db, vertical: true, color: warningColor() }],
                !c.on,
              )
            }}
          />
          <GrBar strip={props.strip} which="comp" on={comp.on} vertical />
        </div>
        <div className="cv-knobs cv-comp-knobs">
          <PKnob
            spec={S.compThreshold}
            value={comp.threshold_db}
            defaultValue={D.comp.threshold_db}
            onChange={(v) => edit((n) => void (n.comp.threshold_db = clamp(v, COMP_THRESHOLD_DB)))}
          />
          <PKnob
            spec={S.compRatio}
            value={comp.ratio}
            defaultValue={D.comp.ratio}
            onChange={(v) => edit((n) => void (n.comp.ratio = clamp(v, COMP_RATIO)))}
          />
          <PKnob
            spec={S.compMakeup}
            value={comp.makeup_db}
            defaultValue={D.comp.makeup_db}
            onChange={(v) => edit((n) => void (n.comp.makeup_db = clamp(v, COMP_MAKEUP_DB)))}
          />
          <PKnob
            spec={S.compAttack}
            value={comp.attack_ms}
            defaultValue={D.comp.attack_ms}
            size={SMALL_KNOB}
            onChange={(v) => edit((n) => void (n.comp.attack_ms = clamp(v, COMP_ATTACK_MS)))}
          />
          <PKnob
            spec={S.compRelease}
            value={comp.release_ms}
            defaultValue={D.comp.release_ms}
            size={SMALL_KNOB}
            onChange={(v) => edit((n) => void (n.comp.release_ms = clamp(v, COMP_RELEASE_MS)))}
          />
          <PKnob
            spec={S.compKnee}
            value={comp.knee_db}
            defaultValue={D.comp.knee_db}
            size={SMALL_KNOB}
            format={(v) => `${v.toFixed(1)} dB`}
            onChange={(v) => edit((n) => void (n.comp.knee_db = clamp(v, COMP_KNEE_DB)))}
          />
        </div>
      </div>
    </Section>
  )
}

// ── Delay ───────────────────────────────────────────────────────────────

/** A number field that commits on Enter or leaving it, and shows the
 *  current value again whenever that changes. */
function NumberField(props: {
  value: number
  digits: number
  unit: string
  label: string
  onCommit: (value: number) => void
}) {
  const text = props.value.toFixed(props.digits)
  return (
    <label className="cv-number" title={props.label}>
      <input
        key={text}
        defaultValue={text}
        inputMode="decimal"
        aria-label={props.label}
        onBlur={(e) => {
          const parsed = Number(e.currentTarget.value.replace(',', '.'))
          if (Number.isFinite(parsed) && e.currentTarget.value.trim() !== '') props.onCommit(parsed)
          else e.currentTarget.value = text
        }}
        onKeyDown={(e) => {
          if (e.key === 'Enter') e.currentTarget.blur()
          if (e.key === 'Escape') {
            e.currentTarget.value = text
            e.currentTarget.blur()
          }
        }}
      />
      <span className="cv-number-unit">{props.unit}</span>
    </label>
  )
}

function DelayCard(props: { p: Processing; edit: EditProcessing }) {
  const { p, edit } = props
  const ms = p.delay.ms
  const setMs = (value: number) => edit((n) => void (n.delay.ms = Math.round(clamp(value, [0, MAX_DELAY_MS]) * 100) / 100))
  return (
    <Section
      title="Delay"
      on={p.delay.on}
      onToggle={() => edit((n) => void (n.delay.on = !n.delay.on))}
      grow={2}
      minWidth={210}
    >
      <div className="cv-knobs">
        <PKnob spec={S.delayMs} value={ms} defaultValue={0} onChange={setMs} />
        <div className="cv-delay-fields">
          <NumberField value={ms} digits={2} unit="ms" label="Delay in milliseconds" onCommit={setMs} />
          <NumberField
            value={ms * SOUND_M_PER_MS}
            digits={2}
            unit="m"
            label="Delay as a distance in metres (343 m/s)"
            onCommit={(metres) => setMs(metres / SOUND_M_PER_MS)}
          />
        </div>
      </div>
      <span className="pe-note">Sound travels {(ms * SOUND_M_PER_MS).toFixed(2)} m in this time (343 m/s).</span>
    </Section>
  )
}

// ── Assignment ──────────────────────────────────────────────────────────

function AssignCard(props: { session: Session; strip: StripRef; core: StripCore }) {
  const { session, strip, core } = props
  // A matrix takes mute groups but no DCA (the engine refuses one).
  const dcas = strip.kind !== 'matrix'
  return (
    <Card title={dcas ? 'DCA and mute groups' : 'Mute groups'} grow={3} minWidth={260}>
      <div className="cv-assign">
        {!dcas && <span className="pe-note">A matrix follows no DCA.</span>}
        {dcas && <span className="knob-caption">DCA</span>}
        {dcas && <div className="assign-grid">
          {session.dcas.map((dca, i) => {
            const on = core.dcas.includes(i)
            return (
              <button
                key={i}
                type="button"
                className={`assign${on ? ' on' : ''}`}
                aria-pressed={on}
                style={chipColorStyle(dca.color)}
                title={`${on ? 'Follows' : 'Follow'} ${dca.name}${dca.mute ? ' (muted)' : ''}`}
                onClick={() => act({ cmd: 'assign_dca', strip, dca: i, assigned: !on })}
              >
                <span className="assign-num">{i + 1}</span>
                <span className="assign-name">{dca.name}</span>
              </button>
            )
          })}
        </div>}
        <span className="knob-caption">Mute groups</span>
        <div className="assign-grid">
          {session.mute_groups.map((group, i) => {
            const on = core.mute_groups.includes(i)
            return (
              <button
                key={i}
                type="button"
                className={`assign assign-mg${on ? ' on' : ''}${group.active ? ' active' : ''}`}
                aria-pressed={on}
                title={`${on ? 'In' : 'Add to'} ${group.name}${group.active ? ' (active: muting)' : ''}`}
                onClick={() => act({ cmd: 'assign_mute_group', strip, group: i, assigned: !on })}
              >
                <span className="assign-num">{i + 1}</span>
                <span className="assign-name">{group.name}</span>
              </button>
            )
          })}
        </div>
      </div>
    </Card>
  )
}

// ── The strip's own fader ───────────────────────────────────────────────

/** Copy, paste and the library for this strip (the selection's, when it is
 *  part of it). */
function ChannelTools(props: { session: Session; strip: StripRef }) {
  const { session, strip } = props
  const clip = useWork((w) => w.clip)
  const phase2 = useStore((s) => s.phase2)
  const targets = targetsFor(session, strip)
  const many = targets.length > 1
  return (
    <div className="cv-tools" role="group" aria-label="Copy and paste">
      <MenuButton label="Copy this strip's settings" icon={<Copy size={14} />} text={<span className="cv-tool-text">Copy</span>} className="button small">
        {(close) => (
          <>
            <MenuItem
              icon={<Copy size={14} />}
              label="Copy processing"
              detail="HPF, gate, EQ, comp, delay"
              onClick={() => {
                close()
                copyDefault(session, strip)
              }}
            />
            <MenuItem
              icon={<ClipboardCopy size={14} />}
              label="Copy…"
              detail="Choose the sections"
              onClick={() => {
                close()
                openDialog({ kind: 'copy', strip })
              }}
            />
          </>
        )}
      </MenuButton>
      <button
        type="button"
        className="button small"
        disabled={!clip || phase2 === false}
        title={
          phase2 === false
            ? 'This server predates paste'
            : clip
              ? `Paste ${pasteLabel(clip)} to ${many ? `the ${targets.length} selected strips` : 'this strip'}`
              : 'Copy a strip first'
        }
        onClick={() => clip && void pasteClip(session, targets, clip)}
      >
        <ClipboardPaste size={14} />
        <span className="cv-tool-text">{many ? `Paste to ${targets.length}` : 'Paste'}</span>
      </button>
      <button
        type="button"
        className="button small"
        title="Library: apply an item to this strip, or save it as one"
        onClick={() => openDialog({ kind: 'library', source: strip, targets })}
      >
        <BookOpen size={14} />
        <span className="cv-tool-text">Library</span>
      </button>
    </div>
  )
}

function SideCard(props: { strip: StripRef; core: StripCore; meterTap: boolean; soloHint: string; soloSilent: boolean }) {
  const { strip, core } = props
  const key = stripKey(strip)
  const phase2 = useStore((s) => s.phase2)
  return (
    <Card className="cv-side-card">
      <div className="cv-side-pan">
        <Knob
          value={core.pan}
          min={-1}
          max={1}
          defaultValue={0}
          bipolar
          size={34}
          label={strip.kind === 'channel' ? 'Pan' : 'Balance'}
          format={formatPan}
          onChange={(pan) => actLatest(`pan:${key}`, { cmd: 'set_pan', strip, pan })}
        />
      </div>
      <Fader
        db={core.fader_db}
        onChange={(db) => actLatest(`fader:${key}`, { cmd: 'set_fader', strip, db })}
        meter={
          <div className="strip-meters">
            {props.meterTap && <Meter strip={key} tap="input" />}
            <Meter strip={key} />
            <GrMeter strip={key} comp={core.processing.comp.on} gate={core.processing.gate.on} />
          </div>
        }
      />
      <div className="strip-row latches">
        <Latch
          kind="mute"
          on={core.mute}
          title="Mute: the strip's output goes silent; pre-fader sends and PFL keep going"
          onClick={() => act({ cmd: 'set_mute', strip, mute: !core.mute })}>
          M
        </Latch>
        {strip.kind !== 'master' && (
          <Latch
            kind="solo"
            on={core.solo}
            title={strip.kind === 'matrix' ? MATRIX_SOLO : `Solo — ${props.soloHint}`}
            onClick={() => act({ cmd: 'set_solo', strip, solo: !core.solo })}
          >
            S
          </Latch>
        )}
      </div>
      {strip.kind !== 'master' && strip.kind !== 'matrix' && (
        <button
          type="button"
          className={`solo-safe${core.solo_safe ? ' on' : ''}`}
          aria-pressed={core.solo_safe}
          title="Solo safe: never silenced by solo in place"
          onClick={() => act({ cmd: 'set_solo_safe', strip, safe: !core.solo_safe })}
        >
          <ShieldCheck size={13} /> Solo safe
        </button>
      )}
      <button
        type="button"
        className={`recall-safe${core.recall_safe ? ' on' : ''}`}
        aria-pressed={core.recall_safe}
        disabled={phase2 === false}
        title={
          phase2 === false
            ? 'This server predates scenes and recall safe'
            : core.recall_safe
              ? 'Recall safe: scene recalls leave this strip alone. Click to let them reach it again'
              : 'Make recall safe: scene recalls will leave this strip entirely alone'
        }
        onClick={() => act({ cmd: 'set_recall_safe', strip, safe: !core.recall_safe })}
      >
        {core.recall_safe ? <Lock size={13} /> : <LockOpen size={13} />} Recall safe
      </button>
      {props.soloSilent && (
        <a className="monitor-unpatched small" href="#patch/outputs" title={props.soloHint}>
          Solo is silent: patch the monitor
        </a>
      )}
      {props.meterTap && (
        <span className="cv-side-note">
          <Mic size={11} /> in · out · GR
        </span>
      )}
    </Card>
  )
}
