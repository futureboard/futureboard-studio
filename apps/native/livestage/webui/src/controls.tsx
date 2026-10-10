// Fader, knob, meter and small buttons: pointer- and touch-driven, for a
// mouse at FOH or a tablet on stage.

import { useEffect, useRef, useState } from 'react'
import type { PointerEvent as ReactPointerEvent, ReactNode } from 'react'
import { alpha } from './editors/paint.ts'
import { dbToPosition, formatDb, meterFraction, positionToDb } from './faderLaw.ts'
import { MAX_FADER_DB, MIN_FADER_DB } from './protocol.ts'
import { meters } from './store.ts'

/** A value the user is moving: shown as they move it, and for a moment
 *  after, until the server's echo has caught up. */
export function useHeld(server: number) {
  const [held, setHeld] = useState<number | null>(null)
  const release = useRef<number | undefined>(undefined)
  useEffect(() => () => window.clearTimeout(release.current), [])
  return {
    value: held ?? server,
    active: held !== null,
    hold(value: number) {
      window.clearTimeout(release.current)
      setHeld(value)
    },
    letGo() {
      window.clearTimeout(release.current)
      release.current = window.setTimeout(() => setHeld(null), 500)
    },
  }
}

/** Vertical drag: calls `move` with the pixels travelled since the press (up
 *  is positive), a quarter speed with Shift held. */
function startDrag(event: ReactPointerEvent<Element>, move: (pixels: number) => void, done: () => void) {
  if (event.button !== 0) return
  event.preventDefault()
  // An SVG knob or an HTML fader: both take pointer listeners alike.
  const element = event.currentTarget as HTMLElement
  element.setPointerCapture(event.pointerId)
  let last = event.clientY
  let travelled = 0
  const onMove = (e: PointerEvent) => {
    travelled += (last - e.clientY) * (e.shiftKey ? 0.25 : 1)
    last = e.clientY
    move(travelled)
  }
  const onUp = () => {
    element.removeEventListener('pointermove', onMove)
    element.removeEventListener('pointerup', onUp)
    element.removeEventListener('pointercancel', onUp)
    done()
  }
  element.addEventListener('pointermove', onMove)
  element.addEventListener('pointerup', onUp)
  element.addEventListener('pointercancel', onUp)
}

const FADER_TICKS = [10, 5, 0, -10, -20, -30, -40, -60]
const THUMB = 38

/** Where fader position `p` (0…1) sits, as a CSS `bottom`: the cap's centre
 *  travels from half a cap above the bottom to half a cap below the top, so
 *  the scale, the ticks and the cap agree at any height without measuring. */
const along = (p: number, offset = THUMB / 2) => `calc(${offset}px + ${p} * (100% - ${THUMB}px))`

/** A fader as tall as its strip leaves room for; `meter` is drawn beside it
 *  at the same height. Click the readout to type a level. */
export function Fader(props: { db: number; onChange: (db: number) => void; meter?: ReactNode }) {
  const [typing, setTyping] = useState(false)
  const held = useHeld(props.db)
  const position = dbToPosition(held.value)

  const set = (db: number) => {
    held.hold(db)
    props.onChange(db)
  }

  return (
    <div className="fader-block">
      <div className="fader">
        <div className="fader-scale">
          {FADER_TICKS.map((tick) => (
            <span
              key={tick}
              className={tick === 0 ? 'unity' : ''}
              style={{ bottom: along(dbToPosition(tick), THUMB / 2 - 5) }}
            >
              {tick > 0 ? `+${tick}` : tick}
            </span>
          ))}
        </div>
        <div
          className={`fader-track${held.active ? ' moving' : ''}`}
          onPointerDown={(e) => {
            const start = position
            const travel = Math.max(1, e.currentTarget.getBoundingClientRect().height - THUMB)
            startDrag(
              e,
              (pixels) => set(positionToDb(Math.min(1, Math.max(0, start + pixels / travel)))),
              () => held.letGo(),
            )
          }}
          onDoubleClick={() => {
            set(0)
            held.letGo()
          }}
          onWheel={(e) => {
            set(positionToDb(position - Math.sign(e.deltaY) * 0.01))
            held.letGo()
          }}
        >
          <div className="fader-groove" style={{ top: THUMB / 2, bottom: THUMB / 2 }} />
          {FADER_TICKS.map((tick) => (
            <div
              key={tick}
              className={`fader-tick${tick === 0 ? ' unity' : ''}`}
              style={{ bottom: along(dbToPosition(tick)) }}
            />
          ))}
          <div className="fader-cap" style={{ bottom: along(position, 0), height: THUMB }}>
            <span />
          </div>
        </div>
        {props.meter}
      </div>
      {typing ? (
        <input
          className="fader-entry"
          autoFocus
          defaultValue={held.value <= MIN_FADER_DB ? '' : held.value.toFixed(1)}
          inputMode="decimal"
          onBlur={() => setTyping(false)}
          onKeyDown={(e) => {
            if (e.key === 'Escape') setTyping(false)
            if (e.key !== 'Enter') return
            const text = e.currentTarget.value.trim().replace('∞', 'inf')
            const parsed = text === '' || /^-?inf/i.test(text) ? MIN_FADER_DB : Number(text)
            if (Number.isFinite(parsed)) {
              set(Math.min(MAX_FADER_DB, Math.max(MIN_FADER_DB, parsed)))
              held.letGo()
            }
            setTyping(false)
          }}
        />
      ) : (
        <button type="button" className="fader-readout" title="Type a level" onClick={() => setTyping(true)}>
          {formatDb(held.value)}
        </button>
      )}
    </div>
  )
}

export function Knob(props: {
  value: number
  min: number
  max: number
  defaultValue: number
  onChange: (value: number) => void
  format: (value: number) => string
  label?: string
  /** Fill from the middle (pan) rather than from the bottom. */
  bipolar?: boolean
  size?: number
  /** Whole steps only. */
  step?: number
  /** Leave the value text off (the caller shows it). */
  hideValue?: boolean
  /** A name between the knob and its value, as a plug-in knob cell has. */
  caption?: string
}) {
  const size = props.size ?? 36
  const held = useHeld(props.value)
  const span = props.max - props.min || 1
  const fraction = Math.min(1, Math.max(0, (held.value - props.min) / span))
  const snap = (v: number) => {
    const clamped = Math.min(props.max, Math.max(props.min, v))
    return props.step ? Math.round(clamped / props.step) * props.step : clamped
  }
  const set = (v: number) => {
    const next = snap(v)
    held.hold(next)
    props.onChange(next)
  }

  // 270° of travel, open at the bottom.
  const angle = (f: number) => (-135 + 270 * f) * (Math.PI / 180)
  const c = size / 2
  const r = size / 2 - 2.5
  const point = (f: number, radius = r) => [c + radius * Math.sin(angle(f)), c - radius * Math.cos(angle(f))]
  const arc = (from: number, to: number) => {
    const [x0, y0] = point(from)
    const [x1, y1] = point(to)
    const large = Math.abs(to - from) * 270 > 180 ? 1 : 0
    return `M ${x0} ${y0} A ${r} ${r} 0 ${large} 1 ${x1} ${y1}`
  }
  // A bipolar arc starts at zero, which is the middle only of a symmetric
  // range (FA-2A's gain runs −12…+24 dB).
  const origin = props.bipolar ? Math.min(1, Math.max(0, -props.min / span)) : 0
  const [px0, py0] = point(fraction, r * 0.28)
  const [px1, py1] = point(fraction, r * 0.62)

  return (
    <div className={`knob${held.active ? ' moving' : ''}`} title={props.label}>
      <svg
        width={size}
        height={size}
        onPointerDown={(e) => {
          const start = held.value
          startDrag(e, (pixels) => set(start + (pixels / 160) * span), () => held.letGo())
        }}
        onDoubleClick={() => {
          set(props.defaultValue)
          held.letGo()
        }}
        onWheel={(e) => {
          set(held.value - Math.sign(e.deltaY) * (props.step ?? span / 100))
          held.letGo()
        }}
      >
        <path className="knob-track" d={arc(0, 1)} />
        {Math.abs(fraction - origin) > 0.002 && (
          <path className="knob-value" d={arc(Math.min(origin, fraction), Math.max(origin, fraction))} />
        )}
        <circle className="knob-body" cx={c} cy={c} r={r * 0.72} />
        <line className="knob-pointer" x1={px0} y1={py0} x2={px1} y2={py1} />
      </svg>
      {props.caption && <span className="knob-caption">{props.caption}</span>}
      {!props.hideValue && <span className="knob-text">{props.format(held.value)}</span>}
    </div>
  )
}

// ── Meters ──────────────────────────────────────────────────────────────

export type Draw = (now: number) => void
const drawers = new Set<Draw>()
let lastFrame = 0
let running = false

/** How long a peak line stays before it falls. */
const HOLD_MS = 1500

function frame(now: number) {
  // The desktop meter's fall: 0.927 per 33 ms.
  const decay = Math.pow(0.927, (now - (lastFrame || now)) / 33.3)
  lastFrame = now
  for (const draw of drawers) draw(now)
  for (const meter of meters.values()) {
    for (const side of [0, 1] as const) {
      meter.output[side] *= decay
      meter.input[side] *= decay
      if (now - meter.outputHoldAt[side] > HOLD_MS) meter.outputHold[side] *= decay * decay
      if (now - meter.inputHoldAt[side] > HOLD_MS) meter.inputHold[side] *= decay * decay
    }
    meter.gateDb *= decay
    meter.compDb *= decay
  }
  if (drawers.size > 0) requestAnimationFrame(frame)
  else running = false
}

const CLIP_HOLD_MS = 2000
const GREEN_TOP = meterFraction(10 ** (-12 / 20))
const YELLOW_TOP = meterFraction(10 ** (-3 / 20))
const SEGMENT = 3
const GAP = 1

/** A stereo LED meter, as tall as the fader beside it. */
export function Meter(props: { strip: string; tap?: 'output' | 'input' }) {
  const canvas = useRef<HTMLCanvasElement>(null)
  const tap = props.tap ?? 'output'
  useEffect(() => {
    const element = canvas.current
    if (!element) return
    const context = element.getContext('2d')
    if (!context) return
    const width = 13
    const styles = getComputedStyle(document.documentElement)
    const color = (name: string) => styles.getPropertyValue(name).trim()
    const lit = [color('--meter-low'), color('--meter-mid'), color('--meter-high')]
    // An unlit segment is its zone's colour, faint, so the scale stays legible.
    const dim = [alpha(lit[0], 0.12), alpha(lit[1], 0.12), alpha(lit[2], 0.14)]
    // The clip light: a hotter red than the meter's top zone (meter.clip).
    const clip = color('--meter-clip')
    const clipDim = alpha(clip, 0.14)
    const capHeight = 4
    const zone = (f: number) => (f <= GREEN_TOP ? 0 : f <= YELLOW_TOP ? 1 : 2)
    // Sized to the element on the frame its height changes, never ahead.
    let height = 0
    let segments = 0
    const segmentTop = (i: number) => height - (i + 1) * (SEGMENT + GAP) + GAP

    const draw: Draw = (now) => {
      if (element.clientHeight !== height) {
        height = element.clientHeight
        const ratio = window.devicePixelRatio || 1
        element.width = width * ratio
        element.height = Math.max(1, height) * ratio
        context.setTransform(ratio, 0, 0, ratio, 0, 0)
        segments = Math.max(0, Math.floor((height - capHeight - 2) / (SEGMENT + GAP)))
      }
      const meter = meters.get(props.strip)
      context.clearRect(0, 0, width, height)
      const clipAt = meter ? (tap === 'output' ? meter.outputClip : meter.inputClip) : -Infinity
      context.fillStyle = now - clipAt < CLIP_HOLD_MS ? clip : clipDim
      context.fillRect(0, 0, width, capHeight)
      for (const side of [0, 1] as const) {
        const x = side * 7
        const level = meterFraction(meter ? meter[tap][side] : 0)
        const hold = meterFraction(meter ? (tap === 'output' ? meter.outputHold : meter.inputHold)[side] : 0)
        for (let i = 0; i < segments; i++) {
          const f = (i + 0.5) / segments
          const z = zone(f)
          context.fillStyle = f <= level ? lit[z] : dim[z]
          context.fillRect(x, segmentTop(i), 6, SEGMENT)
        }
        if (hold > 0.02) {
          const i = Math.min(segments - 1, Math.floor(hold * segments))
          context.fillStyle = lit[zone((i + 0.5) / segments)]
          context.fillRect(x, segmentTop(i), 6, SEGMENT)
        }
      }
    }
    drawers.add(draw)
    if (!running) {
      running = true
      lastFrame = 0
      requestAnimationFrame(frame)
    }
    return () => {
      drawers.delete(draw)
    }
  }, [props.strip, tap])

  return (
    <canvas
      ref={canvas}
      className="meter"
      style={{ width: 13 }}
      title={tap === 'input' ? 'Input after trim. Click to clear the clip light' : 'Click to clear the clip light'}
      onClick={() => {
        const meter = meters.get(props.strip)
        if (meter) {
          meter.outputClip = -Infinity
          meter.inputClip = -Infinity
        }
      }}
    />
  )
}

/** A short horizontal level bar, both sides' louder one (or `side` alone):
 *  enough to see at a glance whether a strip has signal, in a table row. */
export function LevelBar(props: {
  strip: string
  tap?: 'output' | 'input'
  title?: string
  side?: 0 | 1
  width?: number
}) {
  const canvas = useRef<HTMLCanvasElement>(null)
  const tap = props.tap ?? 'output'
  const width = props.width ?? 44
  const side = props.side
  useEffect(() => {
    const element = canvas.current
    if (!element) return
    const context = element.getContext('2d')
    if (!context) return
    const height = 6
    const ratio = window.devicePixelRatio || 1
    element.width = width * ratio
    element.height = height * ratio
    context.setTransform(ratio, 0, 0, ratio, 0, 0)
    const styles = getComputedStyle(document.documentElement)
    const color = (name: string) => styles.getPropertyValue(name).trim()
    const lit = [color('--meter-low'), color('--meter-mid'), color('--meter-high')]
    const track = color('--surface-input')
    const draw: Draw = (now) => {
      const meter = meters.get(props.strip)
      const level = meter
        ? meterFraction(side === undefined ? Math.max(meter[tap][0], meter[tap][1]) : meter[tap][side])
        : 0
      const clipAt = meter ? (tap === 'output' ? meter.outputClip : meter.inputClip) : -Infinity
      context.fillStyle = track
      context.fillRect(0, 0, width, height)
      const zone = now - clipAt < CLIP_HOLD_MS ? 2 : level <= GREEN_TOP ? 0 : level <= YELLOW_TOP ? 1 : 2
      context.fillStyle = lit[zone]
      context.fillRect(0, 0, Math.round(level * width), height)
    }
    drawers.add(draw)
    if (!running) {
      running = true
      lastFrame = 0
      requestAnimationFrame(frame)
    }
    return () => {
      drawers.delete(draw)
    }
  }, [props.strip, tap, width, side])
  return <canvas ref={canvas} className="level-bar" style={{ width, height: 6 }} title={props.title} />
}

/** Adds a per-frame drawer to the meters' animation loop until `undo`. */
export function addDrawer(draw: Draw): () => void {
  drawers.add(draw)
  if (!running) {
    running = true
    lastFrame = 0
    requestAnimationFrame(frame)
  }
  return () => {
    drawers.delete(draw)
  }
}

/** How much a gain-reduction display shows, top to bottom. */
const GR_FULL_DB = 24

/** The processing section's gain reduction beside a strip's meter, hanging
 *  from the top: the compressor's on the left, the gate's on the right, each
 *  only while that section is on. */
export function GrMeter(props: { strip: string; comp: boolean; gate: boolean }) {
  const canvas = useRef<HTMLCanvasElement>(null)
  const { strip, comp, gate } = props
  useEffect(() => {
    const element = canvas.current
    if (!element) return
    const context = element.getContext('2d')
    if (!context) return
    const width = 7
    const styles = getComputedStyle(document.documentElement)
    const compInk = styles.getPropertyValue('--accent').trim()
    const gateInk = styles.getPropertyValue('--text-secondary').trim()
    const track = styles.getPropertyValue('--meter-bg').trim()
    let height = 0
    return addDrawer(() => {
      if (element.clientHeight !== height) {
        height = element.clientHeight
        const ratio = window.devicePixelRatio || 1
        element.width = width * ratio
        element.height = Math.max(1, height) * ratio
        context.setTransform(ratio, 0, 0, ratio, 0, 0)
      }
      const meter = meters.get(strip)
      context.clearRect(0, 0, width, height)
      // Square fills: the lowest lit pixel is the value (DESIGN.md).
      const bar = (x: number, on: boolean, db: number, ink: string) => {
        if (!on) return
        context.fillStyle = track
        context.fillRect(x, 0, 3, height)
        context.fillStyle = ink
        context.fillRect(x, 0, 3, Math.round(Math.min(1, Math.max(0, db / GR_FULL_DB)) * height))
      }
      bar(0, comp, meter?.compDb ?? 0, compInk)
      bar(4, gate, meter?.gateDb ?? 0, gateInk)
    })
  }, [strip, comp, gate])
  return (
    <canvas
      ref={canvas}
      className="gr-meter"
      style={{ width: 7 }}
      title={`Gain reduction, 0 to ${GR_FULL_DB} dB down: compressor ${comp ? '(left)' : 'off'}, gate ${gate ? '(right)' : 'off'}`}
    />
  )
}

/** A gain-reduction bar with its reading, over 24 dB: across (growing from
 *  the right) or down (hanging from the top). The Selected Channel's gate
 *  and compressor. */
export function GrBar(props: { strip: string; which: 'gate' | 'comp'; on: boolean; vertical?: boolean }) {
  const canvas = useRef<HTMLCanvasElement>(null)
  const text = useRef<HTMLSpanElement>(null)
  const { strip, which, on, vertical } = props
  useEffect(() => {
    const element = canvas.current
    if (!element) return
    const context = element.getContext('2d')
    if (!context) return
    const styles = getComputedStyle(document.documentElement)
    const ink = styles.getPropertyValue(which === 'comp' ? '--accent' : '--text-secondary').trim()
    const track = styles.getPropertyValue('--meter-bg').trim()
    const tick = styles.getPropertyValue('--border-normal').trim()
    let w = 0
    let h = 0
    let shown = ''
    return addDrawer(() => {
      if (element.clientWidth !== w || element.clientHeight !== h) {
        w = element.clientWidth
        h = element.clientHeight
        const ratio = window.devicePixelRatio || 1
        element.width = Math.max(1, w) * ratio
        element.height = Math.max(1, h) * ratio
        context.setTransform(ratio, 0, 0, ratio, 0, 0)
      }
      const meter = meters.get(strip)
      const db = on ? ((which === 'comp' ? meter?.compDb : meter?.gateDb) ?? 0) : 0
      const unit = Math.min(1, Math.max(0, db / GR_FULL_DB))
      context.fillStyle = track
      context.fillRect(0, 0, w, h)
      context.fillStyle = tick
      for (const mark of [3, 6, 12]) {
        const at = (mark / GR_FULL_DB) * (vertical ? h : w)
        if (vertical) context.fillRect(0, Math.round(at), w, 1)
        else context.fillRect(Math.round(w - at), 0, 1, h)
      }
      if (on) {
        context.fillStyle = ink
        if (vertical) context.fillRect(0, 0, w, Math.round(unit * h))
        else context.fillRect(Math.round(w - unit * w), 0, Math.round(unit * w), h)
      }
      const next = !on ? 'off' : db >= 0.05 ? `−${db.toFixed(1)}` : '0.0'
      if (next !== shown && text.current) {
        shown = next
        text.current.textContent = next
      }
    })
  }, [strip, which, on, vertical])
  return (
    <div className={`gr-bar${vertical ? ' vertical' : ''}${on ? '' : ' off'}`} title="Gain reduction, dB">
      <canvas ref={canvas} />
      <span ref={text} className="gr-bar-value value">
        —
      </span>
    </div>
  )
}

/** The gate's open light: lit while the gate passes signal, dark while it
 *  holds it down, hollow while the gate is off. Updated each frame without
 *  a re-render. */
export function GateLight(props: { strip: string; on: boolean }) {
  const light = useRef<HTMLSpanElement>(null)
  const { strip, on } = props
  useEffect(() => {
    const element = light.current
    if (!element) return
    let shown: string | null = null
    return addDrawer(() => {
      const next = !on ? 'off' : (meters.get(strip)?.gateOpen ?? true) ? 'open' : 'closed'
      if (next === shown) return
      shown = next
      element.dataset.state = next
      element.title = next === 'off' ? 'Gate off' : next === 'open' ? 'Gate open' : 'Gate closed'
    })
  }, [strip, on])
  return <span ref={light} className="gate-light" />
}

export function Latch(props: {
  on: boolean
  kind: 'mute' | 'solo' | 'arm' | 'plain'
  onClick: () => void
  children: ReactNode
  title?: string
  /** Not latched itself, but in effect (a mute a DCA or mute group holds). */
  implied?: boolean
}) {
  return (
    <button
      type="button"
      className={`latch latch-${props.kind}${props.on ? ' on' : ''}${props.implied && !props.on ? ' implied' : ''}`}
      aria-pressed={props.on}
      title={props.title}
      onClick={props.onClick}
    >
      {props.children}
    </button>
  )
}

/** A native select, dressed: icon on the left, chevron on the right. Native,
 *  so a phone opens its own picker. */
export function Select(props: {
  value: string
  onChange: (value: string) => void
  icon?: ReactNode
  title?: string
  className?: string
  children: ReactNode
}) {
  return (
    <label className={`select${props.className ? ` ${props.className}` : ''}`} title={props.title}>
      {props.icon && <span className="select-icon">{props.icon}</span>}
      <select value={props.value} onChange={(e) => props.onChange(e.currentTarget.value)}>
        {props.children}
      </select>
      <svg className="select-chevron" width="10" height="10" viewBox="0 0 10 10" aria-hidden>
        <path d="M2 3.5 5 6.5 8 3.5" />
      </svg>
    </label>
  )
}
