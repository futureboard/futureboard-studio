import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type CSSProperties,
  type PointerEvent as ReactPointerEvent,
} from 'react'
import { clamp } from '../lib/params'

const ANGLE_START = 135
const ANGLE_SWEEP = 270
/// Pixels of vertical drag for the full range; Shift divides it by five.
const DRAG_SPAN = 200
const WHEEL_STEP = 0.035
const WHEEL_STEP_FINE = 0.006

function polar(radius: number, progress: number) {
  const radians = ((ANGLE_START + ANGLE_SWEEP * clamp(progress, 0, 1)) * Math.PI) / 180
  return [50 + radius * Math.cos(radians), 50 + radius * Math.sin(radians)] as const
}

function arc(from: number, to: number, radius: number) {
  const [lo, hi] = from <= to ? [from, to] : [to, from]
  if (hi - lo < 0.0005) return ''
  const start = polar(radius, lo)
  const end = polar(radius, hi)
  const large = (hi - lo) * ANGLE_SWEEP > 180 ? 1 : 0
  return `M ${start[0]} ${start[1]} A ${radius} ${radius} 0 ${large} 1 ${end[0]} ${end[1]}`
}

export type KnobProps = {
  label: string
  value: number
  min: number
  max: number
  step: number
  unit?: string
  format: (value: number) => string
  defaultValue: number
  /// Fill from the default rather than from the minimum — for bipolar values
  /// such as gain, where "no change" sits in the middle.
  originAtDefault?: boolean
  disabled?: boolean
  disabledHint?: string
  /// Custom value↔travel mapping (log frequency, log Q).
  toProgress?: (value: number) => number
  fromProgress?: (progress: number) => number
  /// Dial diameter in px.
  size?: number
  accent?: string
  onChange: (value: number) => void
}

/// A flat dial: a 270° track, the value arc in the band's colour and a pointer.
/// Drag vertically (Shift for fine), wheel, arrow keys; double-click or
/// right-click resets; click the value to type one.
export function Knob({
  label,
  value,
  min,
  max,
  step,
  unit,
  format,
  defaultValue,
  originAtDefault,
  disabled,
  disabledHint,
  toProgress,
  fromProgress,
  size = 44,
  accent,
  onChange,
}: KnobProps) {
  const dialRef = useRef<HTMLDivElement>(null)
  const gesture = useRef<{ y: number; progress: number; fine: boolean } | null>(null)
  const [dragging, setDragging] = useState(false)
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState('')

  const asProgress = useCallback(
    (raw: number) => clamp(toProgress ? toProgress(raw) : (raw - min) / (max - min), 0, 1),
    [max, min, toProgress],
  )
  const asValue = useCallback(
    (progress: number) => {
      const raw = fromProgress ? fromProgress(progress) : min + progress * (max - min)
      return clamp(Math.round(raw / step) * step, min, max)
    },
    [fromProgress, max, min, step],
  )

  const progress = asProgress(value)
  const origin = originAtDefault ? asProgress(defaultValue) : 0
  const pointerFrom = polar(12, progress)
  const pointerTo = polar(27, progress)

  // Non-passive, so the page does not scroll under a wheel-turned knob.
  useEffect(() => {
    const dial = dialRef.current
    if (!dial || disabled) return
    const onWheel = (event: WheelEvent) => {
      event.preventDefault()
      const amount = event.shiftKey ? WHEEL_STEP_FINE : WHEEL_STEP
      onChange(asValue(clamp(progress + (event.deltaY < 0 ? amount : -amount), 0, 1)))
    }
    dial.addEventListener('wheel', onWheel, { passive: false })
    return () => dial.removeEventListener('wheel', onWheel)
  }, [asValue, disabled, onChange, progress])

  const onPointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (disabled || editing || event.button !== 0) return
    event.preventDefault()
    gesture.current = { y: event.clientY, progress, fine: event.shiftKey }
    event.currentTarget.setPointerCapture(event.pointerId)
    setDragging(true)
  }

  const onPointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    const current = gesture.current
    if (!current || disabled) return
    // Re-anchor when Shift changes mid-drag, so switching to fine never jumps.
    if (current.fine !== event.shiftKey) {
      gesture.current = { y: event.clientY, progress, fine: event.shiftKey }
      return
    }
    const delta = (current.y - event.clientY) / DRAG_SPAN
    onChange(asValue(clamp(current.progress + (current.fine ? delta * 0.2 : delta), 0, 1)))
  }

  const endGesture = (event: ReactPointerEvent<HTMLDivElement>) => {
    gesture.current = null
    setDragging(false)
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId)
    }
  }

  const commitEdit = () => {
    const parsed = parseTyped(draft)
    if (parsed !== null) onChange(clamp(parsed, min, max))
    setEditing(false)
  }

  const readout = `${format(value)}${unit ? ` ${unit}` : ''}`

  return (
    <div
      className={`knob flex min-w-0 flex-col items-center gap-1 ${
        disabled ? 'pointer-events-none opacity-35' : ''
      }`}
      style={{ '--knob-accent': accent } as CSSProperties}
      title={disabled ? disabledHint : undefined}
    >
      <span className="cap">{label}</span>
      <div
        ref={dialRef}
        className="cursor-ns-resize touch-none outline-none"
        style={{ width: size, height: size }}
        role="slider"
        tabIndex={disabled ? -1 : 0}
        aria-label={label}
        aria-valuemin={min}
        aria-valuemax={max}
        aria-valuenow={value}
        aria-valuetext={readout}
        aria-disabled={disabled}
        title={
          disabled
            ? disabledHint
            : `${label} — drag up/down, Shift for fine, double-click to reset`
        }
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={endGesture}
        onPointerCancel={endGesture}
        onDoubleClick={() => !disabled && onChange(defaultValue)}
        onContextMenu={(event) => {
          event.preventDefault()
          if (!disabled) onChange(defaultValue)
        }}
        onKeyDown={(event) => {
          if (disabled) return
          const amount = event.shiftKey ? WHEEL_STEP_FINE : WHEEL_STEP
          if (event.key === 'ArrowUp' || event.key === 'ArrowRight') {
            event.preventDefault()
            onChange(asValue(clamp(progress + amount, 0, 1)))
          } else if (event.key === 'ArrowDown' || event.key === 'ArrowLeft') {
            event.preventDefault()
            onChange(asValue(clamp(progress - amount, 0, 1)))
          } else if (event.key === 'Home') {
            event.preventDefault()
            onChange(min)
          } else if (event.key === 'End') {
            event.preventDefault()
            onChange(max)
          }
        }}
      >
        <svg viewBox="0 0 100 100" className="block h-full w-full overflow-visible" aria-hidden="true">
          <path className="knob-track" d={arc(0, 1, 44)} strokeWidth={7} />
          <path className="knob-fill" d={arc(origin, progress, 44)} strokeWidth={7} />
          <circle className="knob-face" cx="50" cy="50" r="31" />
          <line
            className="knob-pointer"
            x1={pointerFrom[0]}
            y1={pointerFrom[1]}
            x2={pointerTo[0]}
            y2={pointerTo[1]}
            strokeWidth={dragging ? 5 : 4}
          />
        </svg>
      </div>
      {editing ? (
        <input
          className="num h-5 w-16 rounded border border-line-hi bg-canvas text-center text-[11px] text-ink outline-none"
          autoFocus
          value={draft}
          aria-label={`${label} value`}
          onChange={(event) => setDraft(event.target.value)}
          onBlur={commitEdit}
          onKeyDown={(event) => {
            if (event.key === 'Enter') commitEdit()
            if (event.key === 'Escape') setEditing(false)
          }}
        />
      ) : (
        <button
          type="button"
          className="num h-5 min-w-14 cursor-text rounded px-1 text-[11px] font-medium text-ink-2 hover:bg-white/5 hover:text-ink"
          disabled={disabled}
          title="Click to type a value"
          onClick={() => {
            setDraft(String(Number(value.toFixed(2))))
            setEditing(true)
          }}
        >
          {readout}
        </button>
      )}
    </div>
  )
}

/// A typed value: plain numbers, and `k` for thousands ("2.5k" → 2500).
function parseTyped(text: string): number | null {
  const trimmed = text.trim().toLowerCase()
  const match = /^([+-]?\d*\.?\d+)\s*(k)?/.exec(trimmed)
  if (!match) return null
  const base = Number(match[1])
  if (!Number.isFinite(base)) return null
  return match[2] ? base * 1000 : base
}
