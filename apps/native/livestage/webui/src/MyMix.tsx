// Phase 4 §2: the personal monitor mix. A musician's phone on a mic stand:
// one aux bus (or matrix) at a time, a row per channel (or per source) with
// a big horizontal send fader.
//
// `#mix` chooses a mix; `#mix/bus/<id>` and `#mix/matrix/<id>` are one. A
// musician sees only the mixes the server lists as theirs; anyone else may
// pick any aux bus or matrix. Faders never jump to a tap: a drag moves the
// level by how far the finger went, a vertical swipe scrolls the list, and
// a double tap types a value. The server decides what is allowed; its
// refusal is a toast and the fader falls back to the real level.

import { memo, useEffect, useMemo, useRef, useState } from 'react'
import type { CSSProperties, ReactNode, PointerEvent as ReactPointerEvent } from 'react'
import { ChevronLeft, Eye, Grid3x3, Headphones, Pin, TriangleAlert } from 'lucide-react'
import { stripColorStyle } from './Console.tsx'
import type { Draw } from './controls.tsx'
import { Latch, addDrawer, useHeld } from './controls.tsx'
import { alpha } from './editors/paint.ts'
import { dbToPosition, formatDb, formatPan, meterFraction, positionToDb } from './faderLaw.ts'
import type { BusStrip, ChannelStrip, MatrixSource, MatrixStrip, MixRef, Session } from './protocol.ts'
import { MAX_FADER_DB, MIN_FADER_DB, stripKey } from './protocol.ts'
import { matrixSend, matrixSources, roleInfo, sendTo } from './routing.ts'
import { act, actLatest, meters } from './store.ts'
import './mymix.css'

// ── The address ─────────────────────────────────────────────────────────

/** `#mix/bus/3` → the mix; `#mix` (or anything else) → none. */
export function mixFromHash(): MixRef | null {
  const match = /^#mix\/(bus|matrix)\/(\d+)$/.exec(location.hash)
  return match ? { kind: match[1] as 'bus' | 'matrix', id: Number(match[2]) } : null
}

export function mixHash(mix: MixRef | null): string {
  return mix ? `#mix/${mix.kind}/${mix.id}` : '#mix'
}

export function sameMix(a: MixRef, b: MixRef): boolean {
  return a.kind === b.kind && a.id === b.id
}

interface MixChoice {
  mix: MixRef
  name: string
  color: number | null
  detail: string
  /** Gone from the session (a musician's mix that was removed). */
  missing: boolean
}

/** What this page may pick: a musician's own mixes, else every aux bus and
 *  matrix (then the other buses, for a group or FX feed used as a mix). */
export function mixChoices(session: Session, own: MixRef[] | null): MixChoice[] {
  const describe = (mix: MixRef): MixChoice => {
    if (mix.kind === 'bus') {
      const bus = session.buses.find((b) => b.id === mix.id)
      if (!bus) return { mix, name: `Bus ${mix.id}`, color: null, detail: 'No longer in the show', missing: true }
      return {
        mix,
        name: bus.name,
        color: bus.color,
        detail: `${roleInfo(bus.role).label} · ${bus.stereo ? 'stereo' : 'mono'}`,
        missing: false,
      }
    }
    const matrix = session.matrices.find((m) => m.id === mix.id)
    if (!matrix) return { mix, name: `Matrix ${mix.id}`, color: null, detail: 'No longer in the show', missing: true }
    return {
      mix,
      name: matrix.name,
      color: matrix.color,
      detail: `Matrix · ${matrix.stereo ? 'stereo' : 'mono'}`,
      missing: false,
    }
  }
  if (own) return own.map(describe)
  const buses = [...session.buses].sort((a, b) => Number(b.role === 'aux') - Number(a.role === 'aux'))
  return [
    ...buses.filter((b) => b.role === 'aux').map((b) => describe({ kind: 'bus', id: b.id })),
    ...session.matrices.map((m) => describe({ kind: 'matrix', id: m.id })),
  ]
}

// ── "Me" pins (a per-device convenience) ────────────────────────────────

const PINS_KEY = 'livestage.mix.pins'

function loadPins(): string[] {
  try {
    const raw = JSON.parse(localStorage.getItem(PINS_KEY) ?? '[]') as unknown
    return Array.isArray(raw) ? raw.filter((k): k is string => typeof k === 'string') : []
  } catch {
    return []
  }
}

function usePins(): [Set<string>, (key: string) => void] {
  const [pins, setPins] = useState<string[]>(() => loadPins())
  const toggle = (key: string) => {
    const next = pins.includes(key) ? pins.filter((k) => k !== key) : [...pins, key]
    setPins(next)
    try {
      localStorage.setItem(PINS_KEY, JSON.stringify(next))
    } catch {
      // Blocked storage: pins last this page only.
    }
  }
  return [useMemo(() => new Set(pins), [pins]), toggle]
}

// ── The page ────────────────────────────────────────────────────────────

export function MyMixPage(props: {
  session: Session
  mix: MixRef | null
  onMix: (mix: MixRef | null) => void
  /** A musician's own mixes; null: any. */
  own: MixRef[] | null
  readOnly: boolean
}) {
  const { session } = props
  const choices = mixChoices(session, props.own)
  const live = choices.filter((c) => !c.missing)
  // A musician with one mix goes straight to it; a mix this page may not
  // have (or that is gone) falls back to the chooser.
  const allowed = props.mix && choices.some((c) => !c.missing && sameMix(c.mix, props.mix!)) ? props.mix : null
  const shown = allowed ?? (props.own && live.length === 1 ? live[0].mix : null)
  const { onMix } = props
  useEffect(() => {
    if (props.mix && !allowed) onMix(shown)
  }, [props.mix, allowed, shown, onMix])

  if (!shown) return <MixChooser choices={choices} own={props.own !== null} onMix={props.onMix} />
  const back = live.length > 1 || props.own === null ? () => props.onMix(null) : null
  return shown.kind === 'bus' ? (
    <BusMix session={session} bus={shown.id} onBack={back} readOnly={props.readOnly} />
  ) : (
    <MatrixMix session={session} matrix={shown.id} onBack={back} readOnly={props.readOnly} />
  )
}

function MixChooser(props: { choices: MixChoice[]; own: boolean; onMix: (mix: MixRef) => void }) {
  return (
    <div className="mm-page">
      <div className="mm-choose">
        <header className="mm-choose-head">
          <Headphones size={18} />
          <div>
            <h2>{props.own ? 'Your mixes' : 'Choose a mix'}</h2>
            <p>
              {props.own
                ? 'The monitor mixes you may change. Your phone remembers the channels you pin.'
                : 'Any aux bus or matrix, as a musician sees it on their phone.'}
            </p>
          </div>
        </header>
        {props.choices.length === 0 ? (
          <p className="card-empty">
            <TriangleAlert size={15} />
            {props.own
              ? 'No mix is assigned to you yet. Ask the engineer to add one to your user.'
              : 'This show has no aux bus or matrix yet. Add one on the mixer (Aux bank).'}
          </p>
        ) : (
          <div className="mm-choices">
            {props.choices.map((c) => (
              <button
                key={`${c.mix.kind}:${c.mix.id}`}
                type="button"
                className="mm-choice"
                style={stripColorStyle(c.color)}
                disabled={c.missing}
                onClick={() => props.onMix(c.mix)}
              >
                <span className="mm-chip" />
                {c.mix.kind === 'matrix' ? <Grid3x3 size={16} /> : <Headphones size={16} />}
                <span className="mm-choice-text">
                  <strong>{c.name}</strong>
                  <span>{c.detail}</span>
                </span>
              </button>
            ))}
          </div>
        )}
      </div>
    </div>
  )
}

// ── One mix ─────────────────────────────────────────────────────────────

function MixHead(props: {
  strip: BusStrip | MatrixStrip
  kind: 'bus' | 'matrix'
  detail: string
  onBack: (() => void) | null
  readOnly: boolean
}) {
  const { strip } = props
  const ref = { kind: props.kind, id: strip.id } as const
  const key = stripKey(ref)
  return (
    <header className="mm-head" style={stripColorStyle(strip.color)}>
      <div className="mm-head-row">
        {props.onBack && (
          <button type="button" className="icon-button large" title="Other mixes" aria-label="Other mixes" onClick={props.onBack}>
            <ChevronLeft size={18} />
          </button>
        )}
        <span className="mm-chip big" />
        <div className="mm-title">
          <strong>{strip.name}</strong>
          <span>{props.detail}</span>
        </div>
        {props.readOnly && (
          <span className="mm-readonly" title="A viewer sees the mix but cannot change it">
            <Eye size={13} /> View only
          </span>
        )}
        <Latch
          kind="mute"
          on={strip.mute}
          title={strip.mute ? `${strip.name} is muted: tap to unmute` : `Mute ${strip.name}`}
          onClick={() => act({ cmd: 'set_mute', strip: ref, mute: !strip.mute })}
        >
          MUTE
        </Latch>
      </div>
      <div className="mm-master">
        <span className="mm-master-label">Mix</span>
        <HFader
          db={strip.fader_db}
          label={`${strip.name} level`}
          readOnly={props.readOnly}
          onChange={(db) => actLatest(`fader:${key}`, { cmd: 'set_fader', strip: ref, db })}
          meter={<HMeter strip={key} stereo />}
        />
      </div>
    </header>
  )
}

function BusMix(props: { session: Session; bus: number; onBack: (() => void) | null; readOnly: boolean }) {
  const { session } = props
  const bus = session.buses.find((b) => b.id === props.bus)
  const [pins, togglePin] = usePins()
  if (!bus) return null
  const info = roleInfo(bus.role)
  const order = [
    ...session.channels.filter((c) => pins.has(stripKey({ kind: 'channel', id: c.id }))),
    ...session.channels.filter((c) => !pins.has(stripKey({ kind: 'channel', id: c.id }))),
  ]
  const pinned = order.filter((c) => pins.has(stripKey({ kind: 'channel', id: c.id }))).length
  return (
    <div className={`mm-page${props.readOnly ? ' readonly' : ''}`}>
      <MixHead
        strip={bus}
        kind="bus"
        detail={`${info.label} · ${bus.stereo ? 'stereo' : 'mono'} · new sends ${info.preFader ? 'pre' : 'post'}-fader`}
        onBack={props.onBack}
        readOnly={props.readOnly}
      />
      <div className="mm-rows">
        {order.length === 0 && <p className="card-empty">The show has no channels yet.</p>}
        {order.map((channel, i) => (
          <SendRow
            key={channel.id}
            channel={channel}
            bus={bus}
            preDefault={info.preFader}
            pinned={i < pinned}
            groupStart={i === 0 ? (pinned > 0 ? 'Me' : null) : i === pinned ? 'Everyone' : null}
            onPin={() => togglePin(stripKey({ kind: 'channel', id: channel.id }))}
            readOnly={props.readOnly}
          />
        ))}
      </div>
    </div>
  )
}

const SendRow = memo(function SendRow(props: {
  channel: ChannelStrip
  bus: BusStrip
  preDefault: boolean
  pinned: boolean
  groupStart: string | null
  onPin: () => void
  readOnly: boolean
}) {
  const { channel, bus } = props
  const send = sendTo(channel, bus.id)
  const pre = send?.pre_fader ?? props.preDefault
  const level = send?.level_db ?? MIN_FADER_DB
  const base = { cmd: 'set_send' as const, channel: channel.id, bus: bus.id }
  const key = stripKey({ kind: 'channel', id: channel.id })
  // The pan of a send to a stereo bus, once it exists: the channel's own
  // (follow) or its own.
  const pan =
    bus.stereo && send ? (
      <div className="mm-pan">
        <button
          type="button"
          className={`pill mm-follow${send.pan_follow ? ' on' : ''}`}
          aria-pressed={send.pan_follow}
          title={
            send.pan_follow
              ? `Follows ${channel.name}'s own pan (${formatPan(channel.pan)}): tap to set a pan for this mix`
              : 'Its own pan in this mix: tap to follow the channel’s pan again'
          }
          onClick={() => act({ ...base, level_db: level, pan: send.pan, pan_follow: !send.pan_follow })}
        >
          {send.pan_follow ? `Pan ${formatPan(channel.pan)} · follow` : 'Own pan'}
        </button>
        {!send.pan_follow && (
          <HPan
            pan={send.pan}
            label={`${channel.name} pan in ${bus.name}`}
            readOnly={props.readOnly}
            onChange={(p) =>
              actLatest(`sendpan:${channel.id}:${bus.id}`, { ...base, level_db: level, pan: p, pan_follow: false })
            }
          />
        )}
      </div>
    ) : null
  return (
    <>
      {props.groupStart && <div className="mm-group">{props.groupStart}</div>}
      <div className={`mm-row${props.pinned ? ' pinned' : ''}${send ? '' : ' unsent'}`} style={stripColorStyle(channel.color)}>
        <div className="mm-row-name">
          <button
            type="button"
            className={`mm-pin${props.pinned ? ' on' : ''}`}
            aria-pressed={props.pinned}
            title={props.pinned ? 'Unpin: back among everyone' : 'Pin as mine: kept at the top on this device'}
            onClick={props.onPin}
          >
            <Pin size={15} />
          </button>
          <span className="mm-chip" />
          <span className="mm-name" title={channel.name}>
            {channel.name}
          </span>
          <span className="mm-tap" title={pre ? 'Sent before the channel fader' : 'Sent after the channel fader'}>
            {pre ? 'PRE' : 'POST'}
          </span>
        </div>
        <HFader
          db={level}
          label={`${channel.name} in ${bus.name}`}
          readOnly={props.readOnly}
          onChange={(level_db) => actLatest(`send:${channel.id}:${bus.id}`, { ...base, level_db })}
          meter={<HMeter strip={key} tap={pre ? 'input' : 'output'} />}
        />
        {pan}
      </div>
    </>
  )
})

function MatrixMix(props: { session: Session; matrix: number; onBack: (() => void) | null; readOnly: boolean }) {
  const { session } = props
  const matrix = session.matrices.find((m) => m.id === props.matrix)
  const [pins, togglePin] = usePins()
  if (!matrix) return null
  const sources = matrixSources(session)
  const order = [
    ...sources.filter((s) => pins.has(stripKey(s.source))),
    ...sources.filter((s) => !pins.has(stripKey(s.source))),
  ]
  const pinned = order.filter((s) => pins.has(stripKey(s.source))).length
  return (
    <div className={`mm-page${props.readOnly ? ' readonly' : ''}`}>
      <MixHead
        strip={matrix}
        kind="matrix"
        detail={`Matrix · ${matrix.stereo ? 'stereo' : 'mono'} · the master and buses, after their faders`}
        onBack={props.onBack}
        readOnly={props.readOnly}
      />
      <div className="mm-rows">
        {order.map((s, i) => (
          <SourceRow
            key={stripKey(s.source)}
            matrix={matrix}
            source={s.source}
            name={s.name}
            color={s.source.kind === 'bus' ? (session.buses.find((b) => b.id === (s.source as { id: number }).id)?.color ?? null) : null}
            pinned={i < pinned}
            groupStart={i === 0 ? (pinned > 0 ? 'Me' : null) : i === pinned ? 'Everyone' : null}
            onPin={() => togglePin(stripKey(s.source))}
            readOnly={props.readOnly}
          />
        ))}
      </div>
    </div>
  )
}

function SourceRow(props: {
  matrix: MatrixStrip
  source: MatrixSource
  name: string
  color: number | null
  pinned: boolean
  groupStart: string | null
  onPin: () => void
  readOnly: boolean
}) {
  const { matrix, source } = props
  const send = matrixSend(matrix, source)
  const base = { cmd: 'set_matrix_send' as const, matrix: matrix.id, source }
  const key = `${matrix.id}:${stripKey(source)}`
  return (
    <>
      {props.groupStart && <div className="mm-group">{props.groupStart}</div>}
      <div
        className={`mm-row${props.pinned ? ' pinned' : ''}${send.level_db > MIN_FADER_DB ? '' : ' unsent'}`}
        style={stripColorStyle(props.color)}
      >
        <div className="mm-row-name">
          <button
            type="button"
            className={`mm-pin${props.pinned ? ' on' : ''}`}
            aria-pressed={props.pinned}
            title={props.pinned ? 'Unpin' : 'Pin as mine: kept at the top on this device'}
            onClick={props.onPin}
          >
            <Pin size={15} />
          </button>
          <span className="mm-chip" />
          <span className="mm-name">{props.name}</span>
        </div>
        <HFader
          db={send.level_db}
          label={`${props.name} in ${matrix.name}`}
          readOnly={props.readOnly}
          onChange={(level_db) => actLatest(`msend:${key}`, { ...base, level_db, pan: send.pan })}
          meter={<HMeter strip={stripKey(source)} />}
        />
        {matrix.stereo && (
          <div className="mm-pan">
            <span className="mm-pan-label">Pan</span>
            <HPan
              pan={send.pan}
              label={`${props.name} pan in ${matrix.name}`}
              readOnly={props.readOnly}
              onChange={(pan) => actLatest(`msendpan:${key}`, { ...base, level_db: send.level_db, pan })}
            />
          </div>
        )}
      </div>
    </>
  )
}

// ── Horizontal controls ─────────────────────────────────────────────────

/** Two taps this close are a double tap. */
const DOUBLE_TAP_MS = 320
/** A press must travel this far across before it moves anything. */
const SLOP_PX = 4

/** A horizontal drag that moves a value by the distance travelled (never to
 *  where the finger landed). A vertical swipe is the page's scroll (CSS
 *  `touch-action: pan-y`), which cancels the drag. */
function useRelativeDrag(opts: {
  position: number
  onPosition: (position: number) => void
  onEnd: () => void
  onDoubleTap: () => void
  disabled: boolean
}) {
  const latest = useRef(opts)
  latest.current = opts
  const lastTap = useRef(-Infinity)
  return (e: ReactPointerEvent<HTMLDivElement>) => {
    if (e.button !== 0 || latest.current.disabled) return
    const element = e.currentTarget
    const travel = Math.max(1, element.getBoundingClientRect().width)
    const startX = e.clientX
    const start = latest.current.position
    let moving = false
    element.setPointerCapture(e.pointerId)
    const onMove = (m: PointerEvent) => {
      const dx = m.clientX - startX
      if (!moving && Math.abs(dx) < SLOP_PX) return
      moving = true
      const scale = m.shiftKey ? 0.25 : 1
      latest.current.onPosition(Math.min(1, Math.max(0, start + (dx * scale) / travel)))
    }
    const finish = (cancelled: boolean) => {
      element.removeEventListener('pointermove', onMove)
      element.removeEventListener('pointerup', onUp)
      element.removeEventListener('pointercancel', onCancel)
      if (moving) latest.current.onEnd()
      else if (!cancelled) {
        const now = performance.now()
        if (now - lastTap.current < DOUBLE_TAP_MS) {
          lastTap.current = -Infinity
          latest.current.onDoubleTap()
        } else lastTap.current = now
      }
    }
    const onUp = () => finish(false)
    const onCancel = () => finish(true)
    element.addEventListener('pointermove', onMove)
    element.addEventListener('pointerup', onUp)
    element.addEventListener('pointercancel', onCancel)
  }
}

/** A send or mix fader on its side: the desktop's fader law, a cap as big as
 *  a fingertip, the level under it. Double tap (or tap the readout) to type. */
function HFader(props: {
  db: number
  label: string
  onChange: (db: number) => void
  meter?: ReactNode
  readOnly: boolean
}) {
  const held = useHeld(props.db)
  const [typing, setTyping] = useState(false)
  const position = dbToPosition(held.value)
  const set = (db: number) => {
    const rounded = Math.round(db * 10) / 10
    held.hold(rounded)
    props.onChange(rounded)
  }
  const onPointerDown = useRelativeDrag({
    position,
    onPosition: (p) => set(positionToDb(p)),
    onEnd: () => held.letGo(),
    onDoubleTap: () => setTyping(true),
    disabled: props.readOnly,
  })
  const unity = dbToPosition(0)
  return (
    <div className="mm-fader">
      <div
        className={`mm-track${held.active ? ' moving' : ''}${props.readOnly ? ' readonly' : ''}`}
        role="slider"
        tabIndex={props.readOnly ? -1 : 0}
        aria-label={props.label}
        aria-valuemin={MIN_FADER_DB}
        aria-valuemax={MAX_FADER_DB}
        aria-valuenow={Math.round(held.value * 10) / 10}
        aria-valuetext={`${formatDb(held.value)} dB`}
        aria-readonly={props.readOnly}
        style={{ '--pos': position, '--unity': unity } as CSSProperties}
        onPointerDown={onPointerDown}
        onKeyDown={(e) => {
          if (props.readOnly) return
          const step = e.shiftKey ? 0.1 : 1
          if (e.key === 'ArrowRight' || e.key === 'ArrowUp') set(Math.min(MAX_FADER_DB, Math.max(MIN_FADER_DB, held.value) + step))
          else if (e.key === 'ArrowLeft' || e.key === 'ArrowDown') set(Math.max(MIN_FADER_DB, held.value - step))
          else if (e.key === 'Enter') setTyping(true)
          else return
          e.preventDefault()
          held.letGo()
        }}
      >
        <span className="mm-groove" />
        <span className="mm-fill" />
        <span className="mm-unity" title="0 dB" />
        <span className="mm-cap" />
      </div>
      {props.meter}
      {typing ? (
        <input
          className="mm-entry value"
          autoFocus
          defaultValue={held.value <= MIN_FADER_DB ? '' : held.value.toFixed(1)}
          inputMode="decimal"
          enterKeyHint="done"
          aria-label={`${props.label}, dB`}
          onFocus={(e) => e.currentTarget.select()}
          onBlur={() => setTyping(false)}
          onKeyDown={(e) => {
            if (e.key === 'Escape') setTyping(false)
            if (e.key !== 'Enter') return
            const text = e.currentTarget.value.trim().replace('∞', 'inf').replace('−', '-')
            const parsed = text === '' || /^-?inf/i.test(text) ? MIN_FADER_DB : Number(text)
            if (Number.isFinite(parsed)) {
              set(Math.min(MAX_FADER_DB, Math.max(MIN_FADER_DB, parsed)))
              held.letGo()
            }
            setTyping(false)
          }}
        />
      ) : (
        <button
          type="button"
          className="mm-readout value"
          disabled={props.readOnly}
          title="Type a level"
          onClick={() => setTyping(true)}
        >
          {formatDb(held.value)}
        </button>
      )}
    </div>
  )
}

/** A send's pan across: centre is the middle; double tap centres it. */
function HPan(props: { pan: number; label: string; onChange: (pan: number) => void; readOnly: boolean }) {
  const held = useHeld(props.pan)
  const set = (pan: number) => {
    const snapped = Math.abs(pan) < 0.02 ? 0 : Math.round(pan * 100) / 100
    held.hold(snapped)
    props.onChange(snapped)
  }
  const onPointerDown = useRelativeDrag({
    position: (held.value + 1) / 2,
    onPosition: (p) => set(p * 2 - 1),
    onEnd: () => held.letGo(),
    onDoubleTap: () => {
      set(0)
      held.letGo()
    },
    disabled: props.readOnly,
  })
  const at = (held.value + 1) / 2
  return (
    <div className="mm-panner">
      <div
        className={`mm-pan-track${props.readOnly ? ' readonly' : ''}`}
        role="slider"
        tabIndex={props.readOnly ? -1 : 0}
        aria-label={props.label}
        aria-valuemin={-1}
        aria-valuemax={1}
        aria-valuenow={held.value}
        aria-valuetext={formatPan(held.value)}
        style={{ '--from': Math.min(0.5, at), '--to': Math.max(0.5, at), '--pos': at } as CSSProperties}
        title="Drag across · double tap: centre"
        onPointerDown={onPointerDown}
        onKeyDown={(e) => {
          if (props.readOnly) return
          if (e.key === 'ArrowRight') set(Math.min(1, held.value + 0.05))
          else if (e.key === 'ArrowLeft') set(Math.max(-1, held.value - 0.05))
          else return
          e.preventDefault()
          held.letGo()
        }}
      >
        <span className="mm-pan-fill" />
        <span className="mm-pan-mark" />
      </div>
      <span className="mm-pan-value value">{formatPan(held.value)}</span>
    </div>
  )
}

const CLIP_HOLD_MS = 2000
const GREEN_TOP = meterFraction(10 ** (-12 / 20))
const YELLOW_TOP = meterFraction(10 ** (-3 / 20))

/** A level bar as wide as its row (one side, or both stacked), drawn on the
 *  meters' own animation frame. */
function HMeter(props: { strip: string; tap?: 'output' | 'input'; stereo?: boolean }) {
  const canvas = useRef<HTMLCanvasElement>(null)
  const tap = props.tap ?? 'output'
  const stereo = props.stereo === true
  useEffect(() => {
    const element = canvas.current
    const context = element?.getContext('2d')
    if (!element || !context) return
    const styles = getComputedStyle(document.documentElement)
    const color = (name: string) => styles.getPropertyValue(name).trim()
    const lit = [color('--meter-low'), color('--meter-mid'), color('--meter-high')]
    const track = alpha(color('--meter-low'), 0.08)
    const clip = color('--meter-clip')
    let w = 0
    let h = 0
    const draw: Draw = (now) => {
      if (element.clientWidth !== w || element.clientHeight !== h) {
        w = element.clientWidth
        h = element.clientHeight
        const ratio = window.devicePixelRatio || 1
        element.width = Math.max(1, w) * ratio
        element.height = Math.max(1, h) * ratio
        context.setTransform(ratio, 0, 0, ratio, 0, 0)
      }
      const meter = meters.get(props.strip)
      context.clearRect(0, 0, w, h)
      const rows = stereo ? 2 : 1
      const rowH = stereo ? Math.max(1, Math.floor((h - 1) / 2)) : h
      const clipAt = meter ? (tap === 'output' ? meter.outputClip : meter.inputClip) : -Infinity
      for (let side = 0; side < rows; side++) {
        const y = side * (rowH + 1)
        const level = meter
          ? meterFraction(stereo ? meter[tap][side] : Math.max(meter[tap][0], meter[tap][1]))
          : 0
        context.fillStyle = track
        context.fillRect(0, y, w, rowH)
        const zone = now - clipAt < CLIP_HOLD_MS ? -1 : level <= GREEN_TOP ? 0 : level <= YELLOW_TOP ? 1 : 2
        context.fillStyle = zone < 0 ? clip : lit[zone]
        context.fillRect(0, y, Math.round(level * w), rowH)
      }
    }
    return addDrawer(draw)
  }, [props.strip, tap, stereo])
  return (
    <canvas
      ref={canvas}
      className={`mm-meter${stereo ? ' stereo' : ''}`}
      title={tap === 'input' ? 'The channel’s level before its fader' : 'Level after the fader'}
    />
  )
}
