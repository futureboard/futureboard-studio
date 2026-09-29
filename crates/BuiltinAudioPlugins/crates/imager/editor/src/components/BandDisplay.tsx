import {
  useEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type PointerEvent as ReactPointerEvent,
  type RefObject,
} from 'react'
import type { SpectrumFrame } from '../bridge'
import {
  BAND_COUNT,
  BAND_NAMES,
  DEFAULT_CROSSOVERS_HZ,
  DEFAULT_WIDTH,
  MAX_WIDTH,
  SOLO_NONE,
  clamp,
  crossoverBounds,
  formatHz,
  hzToUnit,
  sortedCrossovers,
  unitToHz,
  type ImagerParams,
} from '../lib/params'

const PAD_TOP = 26
const PAD_BOTTOM = 20
/// Gap between a full-width (200 %) band and the plot's edge.
const SHAPE_MARGIN = 8
/// Half the plot's height is 100 % of width travel; Shift is five times finer.
const DRAG_WIDTH_PER_HALF = 100
const FREQ_TICKS = [20, 50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000, 20_000]
const MAJOR_TICKS = new Set([100, 1_000, 10_000])
const CROSSOVER_HIT_PX = 12

type Drag =
  | { kind: 'band'; band: number; y: number; width: number; fine: boolean }
  | { kind: 'crossover'; index: number }

export type BandDisplayProps = {
  params: ImagerParams
  bypassed: boolean
  spectrumRef: RefObject<SpectrumFrame | null>
  onWidth: (band: number, width: number) => void
  /// New crossover set, low to high.
  onCrossovers: (sorted: number[]) => void
}

/// The four bands on a log-frequency axis, over the live input spectrum.
///
/// Each band is drawn as a shape around the centre line whose height is its
/// width: a hairline is mono, the dashed guides are 100 % (unchanged), and
/// the full plot height is 200 %. Drag a band up or down to change its width;
/// drag a crossover line sideways to move it. Double-click either to reset.
export function BandDisplay({
  params,
  bypassed,
  spectrumRef,
  onWidth,
  onCrossovers,
}: BandDisplayProps) {
  const hostRef = useRef<HTMLDivElement>(null)
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const [size, setSize] = useState({ w: 0, h: 0 })
  const [drag, setDrag] = useState<Drag | null>(null)
  const [hoverBand, setHoverBand] = useState<number | null>(null)

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
  const plotH = Math.max(h - PAD_TOP - PAD_BOTTOM, 1)
  const centre = PAD_TOP + plotH / 2
  const half = Math.max(plotH / 2 - SHAPE_MARGIN, 1)
  const x = (hz: number) => hzToUnit(hz) * w
  const sorted = sortedCrossovers(params)
  const edges = [20, ...sorted, 20_000]

  // ---- input spectrum (canvas, redrawn only when a new frame lands) --------
  const geometry = useRef({ w, h })
  useEffect(() => {
    geometry.current = { w, h }
  }, [w, h])

  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas) return
    let drawn: SpectrumFrame | null | undefined = undefined
    let drawnSize = ''
    let raf = 0
    const tick = () => {
      const frame = spectrumRef.current
      const { w: cw, h: ch } = geometry.current
      const key = `${cw}x${ch}`
      if (frame !== drawn || key !== drawnSize) {
        drawSpectrum(canvas, frame, cw, ch)
        drawn = frame
        drawnSize = key
      }
      raf = requestAnimationFrame(tick)
    }
    raf = requestAnimationFrame(tick)
    return () => cancelAnimationFrame(raf)
  }, [spectrumRef])

  // ---- gestures --------------------------------------------------------------
  const onBandDown = (band: number) => (event: ReactPointerEvent<SVGRectElement>) => {
    if (event.button !== 0) return
    event.preventDefault()
    event.currentTarget.setPointerCapture(event.pointerId)
    setDrag({
      kind: 'band',
      band,
      y: event.clientY,
      width: params.width[band]!,
      fine: event.shiftKey,
    })
  }

  const onCrossoverDown = (index: number) => (event: ReactPointerEvent<SVGGElement>) => {
    if (event.button !== 0) return
    event.preventDefault()
    event.currentTarget.setPointerCapture(event.pointerId)
    setDrag({ kind: 'crossover', index })
  }

  const moveCrossover = (index: number, hz: number) => {
    const [lo, hi] = crossoverBounds(sorted, index)
    const next = [...sorted]
    next[index] = clamp(hz, lo, hi)
    onCrossovers(next)
  }

  const onPointerMove = (event: ReactPointerEvent<SVGSVGElement>) => {
    if (!drag) return
    if (drag.kind === 'band') {
      // Re-anchor when Shift changes mid-drag, so switching to fine never jumps.
      if (drag.fine !== event.shiftKey) {
        setDrag({ ...drag, y: event.clientY, width: params.width[drag.band]!, fine: event.shiftKey })
        return
      }
      const travel = ((drag.y - event.clientY) / half) * DRAG_WIDTH_PER_HALF
      const width = clamp(drag.width + (drag.fine ? travel * 0.2 : travel), 0, MAX_WIDTH)
      onWidth(drag.band, Math.round(width * 10) / 10)
    } else {
      const bounds = event.currentTarget.getBoundingClientRect()
      moveCrossover(drag.index, unitToHz((event.clientX - bounds.left) / Math.max(w, 1)))
    }
  }

  const endDrag = (event: ReactPointerEvent<SVGSVGElement>) => {
    if (!drag) return
    setDrag(null)
    const target = event.target as Element
    if (target.hasPointerCapture?.(event.pointerId)) target.releasePointerCapture(event.pointerId)
  }

  const onCrossoverKey = (index: number) => (event: ReactKeyboardEvent<SVGGElement>) => {
    const ratio = event.shiftKey ? 1.005 : 1.03
    if (event.key === 'ArrowRight' || event.key === 'ArrowUp') {
      event.preventDefault()
      moveCrossover(index, sorted[index]! * ratio)
    } else if (event.key === 'ArrowLeft' || event.key === 'ArrowDown') {
      event.preventDefault()
      moveCrossover(index, sorted[index]! / ratio)
    }
  }

  const activeBand = drag?.kind === 'band' ? drag.band : null
  const activeCrossover = drag?.kind === 'crossover' ? drag.index : null
  const unity = (DEFAULT_WIDTH / MAX_WIDTH) * half

  return (
    <div ref={hostRef} className={`bands ${bypassed ? 'is-bypassed' : ''}`}>
      <canvas ref={canvasRef} aria-hidden="true" />
      {w > 0 && h > 0 && (
        <svg
          width={w}
          height={h}
          viewBox={`0 0 ${w} ${h}`}
          onPointerMove={onPointerMove}
          onPointerUp={endDrag}
          onPointerCancel={endDrag}
          role="group"
          aria-label="Bands and crossovers"
        >
          {FREQ_TICKS.map((hz) => (
            <g key={hz}>
              <line
                className={`grid-line ${MAJOR_TICKS.has(hz) ? 'is-major' : ''}`}
                x1={x(hz)}
                x2={x(hz)}
                y1={PAD_TOP}
                y2={PAD_TOP + plotH}
              />
              {hz > 20 && hz < 20_000 && (
                <text className="axis-freq" x={x(hz)} y={h - 6}>
                  {hz >= 1000 ? `${hz / 1000}k` : hz}
                </text>
              )}
            </g>
          ))}
          <line className="grid-line is-centre" x1={0} x2={w} y1={centre} y2={centre} />

          {Array.from({ length: BAND_COUNT }, (_, band) => {
            const x0 = x(edges[band]!)
            const x1 = x(edges[band + 1]!)
            const width = params.width[band]!
            const reach = Math.max((width / MAX_WIDTH) * half, 0.75)
            const muted = params.soloBand !== SOLO_NONE && params.soloBand !== band
            const classes = [
              'band',
              hoverBand === band ? 'is-hover' : '',
              activeBand === band ? 'is-active' : '',
              muted ? 'is-muted' : '',
            ].join(' ')
            return (
              <g key={band} className={classes}>
                <rect
                  className="band-shape"
                  x={x0 + 2}
                  y={centre - reach}
                  width={Math.max(x1 - x0 - 4, 1)}
                  height={reach * 2}
                  rx={3}
                />
                <line className="band-unity" x1={x0 + 2} x2={x1 - 2} y1={centre - unity} y2={centre - unity} />
                <line className="band-unity" x1={x0 + 2} x2={x1 - 2} y1={centre + unity} y2={centre + unity} />
                <rect
                  className="band-hit"
                  x={x0}
                  y={PAD_TOP}
                  width={Math.max(x1 - x0, 1)}
                  height={plotH}
                  onPointerDown={onBandDown(band)}
                  onPointerEnter={() => setHoverBand(band)}
                  onPointerLeave={() => setHoverBand((current) => (current === band ? null : current))}
                  onDoubleClick={() => onWidth(band, DEFAULT_WIDTH)}
                >
                  <title>{`${BAND_NAMES[band]} — drag up/down for width, Shift for fine, double-click for 100 %`}</title>
                </rect>
                {x1 - x0 > 54 && (
                  <text
                    x={(x0 + x1) / 2}
                    y={PAD_TOP - 9}
                    textAnchor="middle"
                    className="num"
                    style={{
                      fill: muted ? 'var(--color-ink-4)' : 'var(--color-ink-2)',
                      fontSize: 10.5,
                      fontWeight: 600,
                      pointerEvents: 'none',
                    }}
                  >
                    {`${BAND_NAMES[band]}  ${Math.round(width)}%`}
                  </text>
                )}
              </g>
            )
          })}

          {sorted.map((hz, index) => {
            const cx = x(hz)
            return (
              <g
                key={index}
                className={`crossover ${activeCrossover === index ? 'is-active' : ''}`}
                role="slider"
                tabIndex={0}
                aria-label={`Crossover ${index + 1}`}
                aria-valuemin={20}
                aria-valuemax={20_000}
                aria-valuenow={Math.round(hz)}
                aria-valuetext={`${formatHz(hz)} Hz`}
                onPointerDown={onCrossoverDown(index)}
                onKeyDown={onCrossoverKey(index)}
                onDoubleClick={() => moveCrossover(index, DEFAULT_CROSSOVERS_HZ[index]!)}
              >
                <title>{`Crossover ${index + 1} — drag sideways, double-click to reset`}</title>
                <rect
                  className="crossover-hit"
                  x={cx - CROSSOVER_HIT_PX / 2}
                  y={PAD_TOP}
                  width={CROSSOVER_HIT_PX}
                  height={plotH}
                />
                <line className="crossover-line" x1={cx} x2={cx} y1={PAD_TOP} y2={PAD_TOP + plotH} />
                <rect
                  x={cx - 22}
                  y={PAD_TOP + plotH - 18}
                  width={44}
                  height={15}
                  rx={4}
                  style={{ fill: 'rgb(20 20 20 / 0.92)', stroke: 'var(--color-line-hi)' }}
                />
                <text
                  x={cx}
                  y={PAD_TOP + plotH - 7}
                  textAnchor="middle"
                  className="num"
                  style={{ fill: 'var(--color-ink)', fontSize: 9.5, pointerEvents: 'none' }}
                >
                  {formatHz(hz)}
                </text>
              </g>
            )
          })}
        </svg>
      )}
    </div>
  )
}

/// Paint one analyser frame as a soft filled area. The frame's own axis
/// (`minHz..maxHz`, log-spaced) is mapped onto the display's, so the two
/// never have to agree on constants.
function drawSpectrum(canvas: HTMLCanvasElement, frame: SpectrumFrame | null, w: number, h: number) {
  const dpr = window.devicePixelRatio || 1
  const pw = Math.max(Math.round(w * dpr), 1)
  const ph = Math.max(Math.round(h * dpr), 1)
  if (canvas.width !== pw || canvas.height !== ph) {
    canvas.width = pw
    canvas.height = ph
  }
  const ctx = canvas.getContext('2d')
  if (!ctx) return
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
  ctx.clearRect(0, 0, w, h)
  if (!frame || frame.bins.length < 2) return

  const plotH = Math.max(h - PAD_TOP - PAD_BOTTOM, 1)
  const base = PAD_TOP + plotH
  const count = frame.bins.length
  const span = Math.log(frame.maxHz / frame.minHz)
  ctx.beginPath()
  ctx.moveTo(0, base)
  for (let i = 0; i < count; i++) {
    const hz = frame.minHz * Math.exp((i / (count - 1)) * span)
    const level = frame.bins[i]! / 255
    ctx.lineTo(hzToUnit(hz) * w, base - level * plotH)
  }
  ctx.lineTo(w, base)
  ctx.closePath()
  const fill = ctx.createLinearGradient(0, PAD_TOP, 0, base)
  fill.addColorStop(0, 'rgba(232, 232, 232, 0.16)')
  fill.addColorStop(1, 'rgba(232, 232, 232, 0.02)')
  ctx.fillStyle = fill
  ctx.fill()
}
