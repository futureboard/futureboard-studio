import {
  useEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type PointerEvent as ReactPointerEvent,
} from 'react'
import {
  DEFAULT_PARAMS,
  MAX_THRESHOLD_DB,
  MIN_THRESHOLD_DB,
  clamp,
  curveReductionDb,
  formatThreshold,
} from '../lib/params'

/// Both axes run over this range, in dBFS.
const FLOOR_DB = -60
const TICKS_DB = [-48, -36, -24, -12]
const PAD = { left: 34, right: 14, top: 16, bottom: 26 }
const THRESHOLD_HIT_PX = 14
const CURVE_POINTS = 160

export type TransferCurveProps = {
  thresholdDb: number
  ratio: number
  kneeDb: number
  /// Linear input peak, from the DSP's meters.
  inputPeak: number
  /// Reduction the stage is applying right now, positive dB.
  reductionDb: number
  /// `false` while the plug-in is bypassed: the curve is shown, the live
  /// operating point is not.
  live: boolean
  onThreshold: (db: number) => void
}

/// The static input→output curve, with the threshold as a handle you drag
/// sideways and the live operating point measured by the DSP: its input peak
/// against its input peak less the reduction actually being applied.
export function TransferCurve({
  thresholdDb,
  ratio,
  kneeDb,
  inputPeak,
  reductionDb,
  live,
  onThreshold,
}: TransferCurveProps) {
  const hostRef = useRef<HTMLDivElement>(null)
  const [size, setSize] = useState({ w: 0, h: 0 })
  const [dragging, setDragging] = useState(false)

  useEffect(() => {
    const host = hostRef.current
    if (!host) return
    const observer = new ResizeObserver((entries) => {
      const rect = entries[0]?.contentRect
      if (rect) setSize({ w: rect.width, h: rect.height })
    })
    observer.observe(host)
    return () => observer.disconnect()
  }, [])

  const { w, h } = size
  // Square plot: the same dB is the same distance on both axes, so the unity
  // diagonal is at 45° and a ratio reads as a slope.
  const side = Math.max(Math.min(w - PAD.left - PAD.right, h - PAD.top - PAD.bottom), 1)
  const left = PAD.left + Math.max((w - PAD.left - PAD.right - side) / 2, 0)
  const top = PAD.top
  const x = (db: number) => left + ((clamp(db, FLOOR_DB, 0) - FLOOR_DB) / -FLOOR_DB) * side
  const y = (db: number) => top + (-clamp(db, FLOOR_DB, 0) / -FLOOR_DB) * side
  const toDb = (px: number) => FLOOR_DB + ((px - left) / side) * -FLOOR_DB

  let curve = ''
  for (let i = 0; i <= CURVE_POINTS; i++) {
    const input = FLOOR_DB + (i / CURVE_POINTS) * -FLOOR_DB
    const output = input - curveReductionDb(input, thresholdDb, ratio, kneeDb)
    curve += `${i === 0 ? 'M' : 'L'}${x(input).toFixed(2)} ${y(output).toFixed(2)}`
  }

  const inputDb = inputPeak > 0 ? 20 * Math.log10(inputPeak) : -Infinity
  const showPoint = live && inputDb > FLOOR_DB
  const pointIn = clamp(inputDb, FLOOR_DB, 0)
  const pointOut = pointIn - Math.max(reductionDb, 0)

  const setFromPointer = (clientX: number, element: Element) => {
    const bounds = element.getBoundingClientRect()
    const db = clamp(toDb(clientX - bounds.left), MIN_THRESHOLD_DB, MAX_THRESHOLD_DB)
    onThreshold(Math.round(db * 10) / 10)
  }

  const onPointerDown = (event: ReactPointerEvent<SVGGElement>) => {
    if (event.button !== 0) return
    event.preventDefault()
    event.currentTarget.setPointerCapture(event.pointerId)
    setDragging(true)
  }

  const onPointerMove = (event: ReactPointerEvent<SVGGElement>) => {
    if (!dragging) return
    const svg = event.currentTarget.ownerSVGElement
    if (svg) setFromPointer(event.clientX, svg)
  }

  const endDrag = (event: ReactPointerEvent<SVGGElement>) => {
    setDragging(false)
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId)
    }
  }

  const onKeyDown = (event: ReactKeyboardEvent<SVGGElement>) => {
    const step = event.shiftKey ? 0.1 : 0.5
    if (event.key === 'ArrowRight' || event.key === 'ArrowUp') {
      event.preventDefault()
      onThreshold(clamp(Math.round((thresholdDb + step) * 10) / 10, MIN_THRESHOLD_DB, MAX_THRESHOLD_DB))
    } else if (event.key === 'ArrowLeft' || event.key === 'ArrowDown') {
      event.preventDefault()
      onThreshold(clamp(Math.round((thresholdDb - step) * 10) / 10, MIN_THRESHOLD_DB, MAX_THRESHOLD_DB))
    }
  }

  const tx = x(thresholdDb)
  const knee = Math.max(kneeDb, 0)

  return (
    <div ref={hostRef} className="curve">
      {w > 0 && h > 0 && (
        <svg width={w} height={h} viewBox={`0 0 ${w} ${h}`} role="group" aria-label="Transfer curve">
          <rect className="plot-floor" x={left} y={top} width={side} height={side} />
          {TICKS_DB.map((db) => (
            <g key={db}>
              <line className="grid-line" x1={x(db)} x2={x(db)} y1={top} y2={top + side} />
              <line className="grid-line" x1={left} x2={left + side} y1={y(db)} y2={y(db)} />
              <text className="axis-label" x={x(db)} y={top + side + 14} textAnchor="middle">
                {db}
              </text>
              <text className="axis-label" x={left - 6} y={y(db) + 3} textAnchor="end">
                {db}
              </text>
            </g>
          ))}
          <text className="axis-caption" x={left + side} y={top + side + 14} textAnchor="end">
            In dB
          </text>
          <text className="axis-caption" x={left - 6} y={top + 8} textAnchor="end">
            Out
          </text>
          <line className="unity-line" x1={x(FLOOR_DB)} y1={y(FLOOR_DB)} x2={x(0)} y2={y(0)} />

          {knee > 0 && (
            <rect
              className="knee-zone"
              x={x(thresholdDb - knee / 2)}
              y={top}
              width={Math.max(x(thresholdDb + knee / 2) - x(thresholdDb - knee / 2), 0)}
              height={side}
            />
          )}

          <path className="curve-line" d={curve} />

          {showPoint && (
            <g className="operating-point" aria-hidden="true">
              <line x1={x(pointIn)} x2={x(pointIn)} y1={y(pointOut)} y2={top + side} />
              <line x1={left} x2={x(pointIn)} y1={y(pointOut)} y2={y(pointOut)} />
              <circle cx={x(pointIn)} cy={y(pointOut)} r={4} />
            </g>
          )}

          <g
            className={`threshold ${dragging ? 'is-active' : ''}`}
            role="slider"
            tabIndex={0}
            aria-label="Threshold"
            aria-valuemin={MIN_THRESHOLD_DB}
            aria-valuemax={MAX_THRESHOLD_DB}
            aria-valuenow={thresholdDb}
            aria-valuetext={`${formatThreshold(thresholdDb)} dB`}
            onPointerDown={onPointerDown}
            onPointerMove={onPointerMove}
            onPointerUp={endDrag}
            onPointerCancel={endDrag}
            onKeyDown={onKeyDown}
            onDoubleClick={() => onThreshold(DEFAULT_PARAMS.thresholdDb)}
          >
            <title>Threshold — drag sideways, double-click to reset</title>
            <rect
              className="threshold-hit"
              x={tx - THRESHOLD_HIT_PX / 2}
              y={top}
              width={THRESHOLD_HIT_PX}
              height={side}
            />
            <line className="threshold-line" x1={tx} x2={tx} y1={top} y2={top + side} />
            <rect className="threshold-tag" x={tx - 26} y={top + 4} width={52} height={16} rx={4} />
            <text className="num threshold-text" x={tx} y={top + 15.5} textAnchor="middle">
              {formatThreshold(thresholdDb)}
            </text>
          </g>
        </svg>
      )}
    </div>
  )
}
