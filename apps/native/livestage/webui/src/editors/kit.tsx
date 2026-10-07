// The frame every built-in editor in the web UI shares: a port of the native
// editors' plugin_kit (components/plugin_kit.rs).
//
// A family editor gets an `Editor`: the insert's values by wire id, edits
// that go straight to the engine, factory presets and A/B, and the insert's
// live readings. It draws itself inside `EditorShell` (title, presets,
// Reset, A/B, Power) from `Card`s, `KitKnob`s, `Choice`s and `LiveCanvas`es.
//
// Values: what the session holds, over the DSP's own defaults; an edit just
// sent shows at once and stays until the session confirms it.

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import type { CSSProperties, MouseEvent, PointerEvent, ReactNode, WheelEvent } from 'react'
import { ChevronLeft, ChevronRight, Power } from 'lucide-react'
import { Knob } from '../controls.tsx'
import type { BuiltinEffect, BuiltinSpec, InsertSlot } from '../protocol.ts'
import { act, actLatest, useStore } from '../store.ts'
import type { KnobSpec } from './knobspec.ts'
import { fromKnob, knobRange, readout, toKnob } from './knobspec.ts'
import type { Live } from './live.ts'
import { useLive } from './live.ts'
import type { Ctx } from './paint.ts'
import './kit.css'

/** The knob size most cards use, and a card's lead control. */
export const KNOB = 34
export const HERO_KNOB = 56

export interface Editor {
  insert: number
  effect: BuiltinEffect
  spec: BuiltinSpec
  /** Whether the effect has wire parameter `id`. */
  has(id: string): boolean
  /** Current value of `id` (its default if the effect lacks it: 0). */
  value(id: string): number
  /** `value(id) >= 0.5`. */
  flag(id: string): boolean
  /** The DSP's own default for `id`. */
  defaultOf(id: string): number
  /** Send one edit. A drag may call this every frame: edits to one id are
   *  coalesced to the latest per frame. */
  set(id: string, value: number): void
  /** Several edits as one (a preset, a reset of a band). */
  setMany(values: Record<string, number>): void
  /** Every value, by wire index. */
  values(): number[]
  /** The insert's live readings. */
  live: Live
  /** True while the insert is bypassed in LiveStage or its own `power` is
   *  off: displays dim, as the native ones do. */
  bypassed: boolean
  /** The engine's sample rate, for displays drawn from the DSP's filters. */
  sampleRate: number
}

export type EditorComponent = (props: { editor: Editor }) => ReactNode

// Presets and A/B per insert, kept while the page lives (as a native editor
// window keeps them while it is open).
interface Session {
  preset: number | null
  other: number[] | null
  onB: boolean
}
const sessions = new Map<number, Session>()
function sessionOf(insert: number): Session {
  let s = sessions.get(insert)
  if (!s) {
    s = { preset: null, other: null, onB: false }
    sessions.set(insert, s)
  }
  return s
}

/** How long an edit just sent outranks the session's value. */
const PENDING_MS = 1500

export function useEditor(slot: InsertSlot, effect: BuiltinEffect, spec: BuiltinSpec): Editor {
  const live = useLive(slot.id)
  const sampleRate = useStore((s) => s.status?.status.sample_rate ?? 48000)
  const [, setVersion] = useState(0)
  const bump = useCallback(() => setVersion((v) => v + 1), [])
  const pending = useRef(new Map<number, { value: number; at: number }>())
  const index = useMemo(() => new Map(spec.ids.map((id, i) => [id, i])), [spec])
  const stored = useMemo(
    () => new Map(slot.plugin.type === 'builtin' ? slot.plugin.params : []),
    [slot.plugin],
  )

  const valueAt = (i: number): number => {
    const p = pending.current.get(i)
    const session = stored.get(i)
    if (p) {
      const confirmed = session !== undefined && Math.abs(session - p.value) < 1e-6
      if (!confirmed && performance.now() - p.at < PENDING_MS) return p.value
      pending.current.delete(i)
    }
    return session ?? spec.defaults[i] ?? 0
  }

  const send = (i: number, value: number) => {
    pending.current.set(i, { value, at: performance.now() })
    actLatest(`param:${slot.id}:${i}`, { cmd: 'set_insert_param', insert: slot.id, index: i, value })
  }

  const editor: Editor = {
    insert: slot.id,
    effect,
    spec,
    live,
    sampleRate,
    bypassed: false,
    has: (id) => index.has(id),
    value: (id) => {
      const i = index.get(id)
      return i === undefined ? 0 : valueAt(i)
    },
    flag: (id) => editor.value(id) >= 0.5,
    defaultOf: (id) => {
      const i = index.get(id)
      return i === undefined ? 0 : (spec.defaults[i] ?? 0)
    },
    set: (id, value) => {
      const i = index.get(id)
      if (i === undefined || !Number.isFinite(value)) return
      sessionOf(slot.id).preset = null
      send(i, value)
      bump()
    },
    setMany: (values) => {
      const changes: [number, number][] = []
      for (const [id, value] of Object.entries(values)) {
        const i = index.get(id)
        if (i !== undefined && Number.isFinite(value)) changes.push([i, value])
      }
      applyValues(changes)
      sessionOf(slot.id).preset = null
    },
    values: () => spec.ids.map((_, i) => valueAt(i)),
  }

  /** Sends what differs, in one command. */
  function applyValues(changes: [number, number][]) {
    const now = performance.now()
    const differing = changes.filter(([i, v]) => Math.abs(valueAt(i) - v) > 1e-7)
    if (differing.length === 0) return
    for (const [i, v] of differing) pending.current.set(i, { value: v, at: now })
    act({ cmd: 'set_insert_params', insert: slot.id, values: differing })
    bump()
  }
  editor.bypassed = slot.bypass || (index.has('power') && !editor.flag('power'))
  // The shell reaches these through the editor.
  ;(editor as EditorInternals).applyValues = applyValues
  ;(editor as EditorInternals).bump = bump
  return editor
}

interface EditorInternals extends Editor {
  applyValues(changes: [number, number][]): void
  bump(): void
}

// ── The shell ───────────────────────────────────────────────────────────

/** Ids a preset leaves alone: what you are listening to, not the sound. */
const LISTENING = ['power']

export function EditorShell(props: {
  editor: Editor
  title: string
  subtitle?: string
  /** Ids a preset must not change, beyond `power` (A/B swaps them). */
  keep?: string[]
  children: ReactNode
  className?: string
}) {
  const editor = props.editor as EditorInternals
  const { spec } = editor
  const session = sessionOf(editor.insert)
  const presets = spec.presets
  const keep = new Set([...LISTENING, ...(props.keep ?? [])])
  const [notice, setNotice] = useState<string | null>(null)
  useEffect(() => {
    if (!notice) return
    const timer = window.setTimeout(() => setNotice(null), 2200)
    return () => window.clearTimeout(timer)
  }, [notice])

  // A preset leaves `keep` alone; an A/B swap changes everything but power,
  // as the native shell does.
  const listening = new Set(LISTENING)
  const apply = (values: number[], kept: Set<string> = keep) => {
    const current = editor.values()
    editor.applyValues(
      values
        .map((v, i): [number, number] => [i, kept.has(spec.ids[i]) ? current[i] : v])
        .filter(([i, v]) => v !== current[i]),
    )
  }
  const loadPreset = (i: number) => {
    if (presets.length === 0) return
    const wrapped = ((i % presets.length) + presets.length) % presets.length
    apply(presets[wrapped].values)
    session.preset = wrapped
    editor.bump()
  }
  const step = (delta: number) => {
    const from = session.preset ?? (delta > 0 ? -1 : 0)
    loadPreset(from + delta)
  }
  const reset = () => {
    if (presets.length > 0) loadPreset(0)
    else {
      apply(spec.defaults)
      session.preset = null
    }
  }
  const switchTo = (b: boolean) => {
    if (b === session.onB) return
    const current = editor.values()
    if (session.other) apply(session.other, listening)
    session.other = current
    session.onB = b
    session.preset = null
    editor.bump()
  }
  const copy = () => {
    session.other = editor.values()
    setNotice(session.onB ? 'Copied B to A' : 'Copied A to B')
  }
  const hasPower = editor.has('power')
  // Values that are exactly a preset (a fresh insert is the first one) show
  // its name rather than "Edited".
  if (session.preset === null && presets.length > 0) {
    const current = editor.values()
    const same = (values: number[]) =>
      values.every((v, i) => keep.has(spec.ids[i]) || Math.abs(v - current[i]) < 1e-4)
    const found = presets.findIndex((preset) => same(preset.values))
    if (found >= 0) session.preset = found
  }

  return (
    <div className={`pe${props.className ? ` ${props.className}` : ''}${editor.bypassed ? ' pe-off' : ''}`}>
      <header className="pe-head">
        <div className="pe-title">
          <strong>{props.title}</strong>
          {props.subtitle && <span>{props.subtitle}</span>}
        </div>
        {presets.length > 0 && (
          <div className="pe-presets">
            <button type="button" className="icon-button large" title="Previous preset" onClick={() => step(-1)}>
              <ChevronLeft size={15} />
            </button>
            <label className="select pe-preset-select">
              <select
                value={session.preset ?? ''}
                onChange={(e) => e.currentTarget.value !== '' && loadPreset(Number(e.currentTarget.value))}
              >
                {session.preset === null && <option value="">Edited</option>}
                {presets.map((preset, i) => (
                  <option key={preset.name} value={i}>
                    {preset.name}
                  </option>
                ))}
              </select>
              <svg className="select-chevron" width="10" height="10" viewBox="0 0 10 10" aria-hidden>
                <path d="M2 3.5 5 6.5 8 3.5" />
              </svg>
            </label>
            <button type="button" className="icon-button large" title="Next preset" onClick={() => step(1)}>
              <ChevronRight size={15} />
            </button>
          </div>
        )}
        <button type="button" className="button ghost pe-reset" onClick={reset}>
          Reset
        </button>
        <div className="segments pe-ab">
          <button type="button" className={session.onB ? '' : 'on'} onClick={() => switchTo(false)}>
            A
          </button>
          <button type="button" className={session.onB ? 'on' : ''} onClick={() => switchTo(true)}>
            B
          </button>
        </div>
        <button type="button" className="button ghost" onClick={copy}>
          {session.onB ? 'Copy B → A' : 'Copy A → B'}
        </button>
        {notice && <span className="pe-notice">{notice}</span>}
        <span className="spacer" />
        {hasPower && (
          <button
            type="button"
            className={`switch${editor.flag('power') ? ' on' : ''}`}
            onClick={() => editor.set('power', editor.flag('power') ? 0 : 1)}
          >
            <Power size={13} strokeWidth={2.5} /> Power
          </button>
        )}
      </header>
      <div className="pe-body">{props.children}</div>
    </div>
  )
}

// ── Pieces ──────────────────────────────────────────────────────────────

/** A card: a caption over its controls. Takes a share of its row by `grow`
 *  and never gets narrower than `minWidth`. */
export function Card(props: {
  title?: ReactNode
  /** Right of the caption: a range, a toggle. */
  aside?: ReactNode
  grow?: number
  minWidth?: number
  className?: string
  style?: CSSProperties
  children?: ReactNode
}) {
  return (
    <section
      className={`pe-card${props.className ? ` ${props.className}` : ''}`}
      style={{ flexGrow: props.grow ?? 1, minWidth: props.minWidth, ...props.style }}
    >
      {(props.title || props.aside) && (
        <div className="pe-card-head">
          <span>{props.title}</span>
          {props.aside}
        </div>
      )}
      {props.children}
    </section>
  )
}

/** A row of cards (or anything), wrapping on a narrow screen. */
export function Row(props: { children: ReactNode; className?: string; style?: CSSProperties }) {
  return (
    <div className={`pe-row${props.className ? ` ${props.className}` : ''}`} style={props.style}>
      {props.children}
    </div>
  )
}

/** A knob on wire parameter `spec.id`, turning and reading as `spec` says. */
export function KitKnob(props: { editor: Editor; spec: KnobSpec; size?: number; label?: string }) {
  const { editor, spec } = props
  const [lo, hi] = knobRange(spec)
  return (
    <div className="pe-knob">
      <Knob
        value={toKnob(spec, editor.value(spec.id))}
        min={lo}
        max={hi}
        defaultValue={toKnob(spec, editor.defaultOf(spec.id))}
        bipolar={spec.bipolar}
        size={props.size ?? KNOB}
        label={`${props.label ?? spec.label} (double-click: default)`}
        caption={props.label ?? spec.label}
        format={(units) => readout(spec, fromKnob(spec, units))}
        onChange={(units) => editor.set(spec.id, fromKnob(spec, units))}
      />
    </div>
  )
}

/** Segmented choice. */
export function Choice<T extends string | number>(props: {
  value: T
  options: readonly (readonly [T, string])[]
  onChange: (value: T) => void
  className?: string
  title?: string
}) {
  return (
    <div className={`segments pe-choice${props.className ? ` ${props.className}` : ''}`} title={props.title}>
      {props.options.map(([value, text]) => (
        <button
          key={String(value)}
          type="button"
          className={value === props.value ? 'on' : ''}
          onClick={() => props.onChange(value)}
        >
          {text}
        </button>
      ))}
    </div>
  )
}

/** A wire parameter as a segmented choice of its whole-number values. */
export function ParamChoice(props: {
  editor: Editor
  id: string
  options: readonly (readonly [number, string])[]
  className?: string
}) {
  const current = Math.round(props.editor.value(props.id))
  return (
    <Choice
      value={current}
      options={props.options}
      className={props.className}
      onChange={(v) => props.editor.set(props.id, v)}
    />
  )
}

/** A checkbox with its label. */
export function Check(props: { label: ReactNode; checked: boolean; onChange: (checked: boolean) => void; title?: string }) {
  return (
    <label className="pe-check" title={props.title}>
      <input type="checkbox" checked={props.checked} onChange={(e) => props.onChange(e.currentTarget.checked)} />
      <span>{props.label}</span>
    </label>
  )
}

/** A wire flag as a checkbox. */
export function ParamCheck(props: { editor: Editor; id: string; label: ReactNode }) {
  return (
    <Check
      label={props.label}
      checked={props.editor.flag(props.id)}
      onChange={(on) => props.editor.set(props.id, on ? 1 : 0)}
    />
  )
}

/** A small latching button (Bypass, Solo, Link). */
export function Toggle(props: { on: boolean; onClick: () => void; children: ReactNode; title?: string; className?: string }) {
  return (
    <button
      type="button"
      className={`pe-toggle${props.on ? ' on' : ''}${props.className ? ` ${props.className}` : ''}`}
      aria-pressed={props.on}
      title={props.title}
      onClick={props.onClick}
    >
      {props.children}
    </button>
  )
}

// ── Live displays ───────────────────────────────────────────────────────

type Painter = (now: number) => void
const painters = new Set<Painter>()
let looping = false

function loop(now: number) {
  for (const paint of painters) paint(now)
  if (painters.size > 0) requestAnimationFrame(loop)
  else looping = false
}

/** A canvas that fills its box and redraws every animation frame with
 *  `draw(ctx, width, height, now)`, in CSS pixels. For meters, needles,
 *  scopes and curves; the box's own CSS sets its size. Pointer handlers
 *  get coordinates relative to the box. */
export function LiveCanvas(props: {
  draw: (ctx: Ctx, w: number, h: number, now: number) => void
  className?: string
  style?: CSSProperties
  onPointerDown?: (x: number, y: number, e: PointerEvent<HTMLDivElement>) => void
  onPointerMove?: (x: number, y: number, e: PointerEvent<HTMLDivElement>) => void
  onPointerUp?: (x: number, y: number, e: PointerEvent<HTMLDivElement>) => void
  onDoubleClick?: (x: number, y: number, e: MouseEvent<HTMLDivElement>) => void
  onWheel?: (x: number, y: number, e: WheelEvent<HTMLDivElement>) => void
  children?: ReactNode
}) {
  const box = useRef<HTMLDivElement>(null)
  const canvas = useRef<HTMLCanvasElement>(null)
  const draw = useRef(props.draw)
  draw.current = props.draw
  useLayoutEffect(() => {
    const element = canvas.current
    const container = box.current
    if (!element || !container) return
    const context = element.getContext('2d')
    if (!context) return
    let width = 0
    let height = 0
    const paint: Painter = (now) => {
      const w = container.clientWidth
      const h = container.clientHeight
      const ratio = window.devicePixelRatio || 1
      if (w !== width || h !== height) {
        width = w
        height = h
        element.width = Math.max(1, Math.round(w * ratio))
        element.height = Math.max(1, Math.round(h * ratio))
      }
      context.setTransform(ratio, 0, 0, ratio, 0, 0)
      context.clearRect(0, 0, w, h)
      if (w > 0 && h > 0) draw.current(context, w, h, now)
    }
    painters.add(paint)
    if (!looping) {
      looping = true
      requestAnimationFrame(loop)
    }
    return () => {
      painters.delete(paint)
    }
  }, [])
  const at = (e: { clientX: number; clientY: number }): [number, number] => {
    const r = box.current!.getBoundingClientRect()
    return [e.clientX - r.left, e.clientY - r.top]
  }
  return (
    <div
      ref={box}
      className={`pe-canvas${props.className ? ` ${props.className}` : ''}`}
      style={props.style}
      onPointerDown={
        props.onPointerDown &&
        ((e) => {
          e.currentTarget.setPointerCapture(e.pointerId)
          props.onPointerDown!(...at(e), e)
        })
      }
      onPointerMove={props.onPointerMove && ((e) => props.onPointerMove!(...at(e), e))}
      onPointerUp={props.onPointerUp && ((e) => props.onPointerUp!(...at(e), e))}
      onDoubleClick={props.onDoubleClick && ((e) => props.onDoubleClick!(...at(e), e))}
      onWheel={props.onWheel && ((e) => props.onWheel!(...at(e), e))}
    >
      <canvas ref={canvas} />
      {props.children}
    </div>
  )
}

/** A caption chip over a display ("LEVEL HISTORY", "10 s"). */
export function DisplayTag(props: { children: ReactNode; right?: boolean }) {
  return <span className={`pe-tag${props.right ? ' right' : ''}`}>{props.children}</span>
}
