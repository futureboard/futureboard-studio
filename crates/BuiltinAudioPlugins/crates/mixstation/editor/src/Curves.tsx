import { useMemo, useState, type PointerEvent as ReactPointerEvent, type ReactNode, type RefObject } from 'react'
import type { SpectrumFrame } from './bridge'
import { clamp } from './math'
import { PARAM_SPECS, type NumericParamId } from './params'
import {
  DISPLAY_SAMPLE_RATE,
  HIGH_SHELF_HZ,
  LOW_SHELF_HZ,
  chainMagnitudeDb,
  compressorOutputDb,
  eqSections,
  filterSections,
  limiterOutputDb,
  saturate,
  stereoWidth,
} from './response'
import { SpectrumOverlay } from './Spectrum'
import { useMeasure } from './useMeasure'
import type { MixStationParams } from './bridge'

const F_MIN = 20
const F_MAX = 20_000
const FREQ_TICKS = [50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000]
const MAJOR_FREQ = new Set([100, 1_000, 10_000])
const PAD = 14
const HANDLE_INSET = 8

export type CurveChange = (id: NumericParamId, value: number) => void

type Box = { w: number; h: number }

const freqX = (hz: number, w: number) => (Math.log(clamp(hz, F_MIN, F_MAX) / F_MIN) / Math.log(F_MAX / F_MIN)) * w
const xFreq = (x: number, w: number) => F_MIN * Math.pow(F_MAX / F_MIN, clamp(x / Math.max(w, 1), 0, 1))
const freqLabel = (hz: number) => (hz >= 1000 ? `${hz / 1000}k` : `${hz}`)

function path(points: [number, number][]) {
  return points.map(([x, y], i) => `${i === 0 ? 'M' : 'L'}${x.toFixed(1)} ${y.toFixed(1)}`).join('')
}

/// The frame every display sits in: a measured surface, one accent trace.
function Plot({ label, children }: { label: string; children: (box: Box) => ReactNode }) {
  const [ref, box] = useMeasure<HTMLDivElement>()
  return (
    <div ref={ref} role="group" aria-label={label} className="relative h-full w-full overflow-hidden">
      {children(box)}
    </div>
  )
}

function FrequencyGrid({ w, h, dbTicks, y }: { w: number; h: number; dbTicks: number[]; y: (db: number) => number }) {
  return (
    <>
      {FREQ_TICKS.map((hz) => (
        <g key={hz}>
          <line className={`plot-grid ${MAJOR_FREQ.has(hz) ? 'is-major' : ''}`} x1={freqX(hz, w)} x2={freqX(hz, w)} y1={0} y2={h} />
          <text className="plot-label" x={freqX(hz, w)} y={h - 4} textAnchor="middle">
            {freqLabel(hz)}
          </text>
        </g>
      ))}
      {dbTicks.map((db) => (
        <g key={db}>
          <line className={`plot-grid ${db === 0 ? 'is-major' : ''}`} x1={0} x2={w} y1={y(db)} y2={y(db)} />
          <text className="plot-label" x={w - 4} y={y(db) - 3} textAnchor="end">
            {db > 0 ? `+${db}` : db}
          </text>
        </g>
      ))}
    </>
  )
}

/// Pointer capture for a handle dragged across a plot, in the plot's pixels.
function useDrag(onMove: (x: number, y: number) => void) {
  const [active, setActive] = useState(false)
  return {
    active,
    handlers: {
      onPointerDown: (event: ReactPointerEvent<SVGGElement>) => {
        if (event.button !== 0) return
        event.preventDefault()
        event.currentTarget.setPointerCapture(event.pointerId)
        setActive(true)
      },
      onPointerMove: (event: ReactPointerEvent<SVGGElement>) => {
        if (!active) return
        const svg = event.currentTarget.ownerSVGElement
        if (!svg) return
        const bounds = svg.getBoundingClientRect()
        onMove(event.clientX - bounds.left, event.clientY - bounds.top)
      },
      onPointerUp: (event: ReactPointerEvent<SVGGElement>) => {
        setActive(false)
        if (event.currentTarget.hasPointerCapture(event.pointerId)) {
          event.currentTarget.releasePointerCapture(event.pointerId)
        }
      },
      onPointerCancel: () => setActive(false),
    },
  }
}

function Handle({
  x,
  y,
  bounds,
  label,
  title,
  drag,
  onReset,
}: {
  x: number
  y: number
  /// The plot's size: the handle is kept `HANDLE_INSET` inside it, so a value
  /// at the end of its range is still visible and grabbable.
  bounds: Box
  label: string
  title: string
  drag: ReturnType<typeof useDrag>
  onReset: () => void
}) {
  const cx = clamp(x, HANDLE_INSET, Math.max(bounds.w - HANDLE_INSET, HANDLE_INSET))
  const cy = clamp(y, HANDLE_INSET + 12, Math.max(bounds.h - HANDLE_INSET, HANDLE_INSET + 12))
  const anchor = cx < 40 ? 'start' : cx > bounds.w - 40 ? 'end' : 'middle'
  const labelX = anchor === 'start' ? -6 : anchor === 'end' ? 6 : 0
  return (
    <g className="plot-handle" transform={`translate(${cx} ${cy})`} {...drag.handlers} onDoubleClick={onReset}>
      <title>{title}</title>
      <circle r={13} fill="transparent" />
      <circle r={drag.active ? 7 : 6} fill="var(--color-accent)" stroke="var(--color-floor)" strokeWidth={2} />
      <text x={labelX} y={-11} textAnchor={anchor} className="plot-label" style={{ fill: 'var(--color-ink-2)', fontWeight: 600 }}>
        {label}
      </text>
    </g>
  )
}

// ── Filters ────────────────────────────────────────────────────────────────

/// Combined 24 dB/oct low and high cut, from the coefficients Rust builds.
/// Each corner is a handle you drag sideways.
export function FilterDisplay({
  params,
  active,
  spectrumRef,
  spectrumLive,
  onChange,
}: {
  params: MixStationParams
  active: boolean
  spectrumRef: RefObject<SpectrumFrame | null>
  spectrumLive: boolean
  onChange: CurveChange
}) {
  return (
    <Plot label="Low and high cut response — drag a corner sideways">
      {({ w, h }) => (
        <>
          <SpectrumOverlay frameRef={spectrumRef} live={spectrumLive} minHz={F_MIN} maxHz={F_MAX} />
          <FilterSvg w={w} h={h} params={params} active={active} onChange={onChange} />
        </>
      )}
    </Plot>
  )
}

function FilterSvg({ w, h, params, active, onChange }: Box & { params: MixStationParams; active: boolean; onChange: CurveChange }) {
  const y = (db: number) => PAD + ((6 - clamp(db, -36, 6)) / 42) * (h - PAD * 2)
  const curve = useMemo(() => {
    const sections = filterSections(params.hpfHz, params.lpfHz, DISPLAY_SAMPLE_RATE)
    const points: [number, number][] = []
    for (let i = 0; i <= 160; i++) {
      const hz = F_MIN * Math.pow(F_MAX / F_MIN, i / 160)
      points.push([freqX(hz, w), y(chainMagnitudeDb(sections, DISPLAY_SAMPLE_RATE, hz))])
    }
    return path(points)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [params.hpfHz, params.lpfHz, w, h])
  const setHz = (id: 'hpfHz' | 'lpfHz') => (x: number) => {
    const spec = PARAM_SPECS[id]
    onChange(id, clamp(xFreq(x, w), spec.min, spec.max))
  }
  const hpf = useDrag((x) => setHz('hpfHz')(x))
  const lpf = useDrag((x) => setHz('lpfHz')(x))
  return (
    <svg className="relative h-full w-full touch-none" viewBox={`0 0 ${w} ${h}`}>
      <FrequencyGrid w={w} h={h} dbTicks={[0, -12, -24]} y={y} />
      <path className={`plot-curve ${active ? '' : 'is-idle'}`} d={curve} />
      <Handle
        bounds={{ w, h }}
        x={freqX(params.hpfHz, w)}
        y={y(-3)}
        label="Low cut"
        title="Low cut — drag sideways, double-click to open"
        drag={hpf}
        onReset={() => onChange('hpfHz', PARAM_SPECS.hpfHz.defaultValue)}
      />
      <Handle
        bounds={{ w, h }}
        x={freqX(params.lpfHz, w)}
        y={y(-3)}
        label="High cut"
        title="High cut — drag sideways, double-click to open"
        drag={lpf}
        onReset={() => onChange('lpfHz', PARAM_SPECS.lpfHz.defaultValue)}
      />
    </svg>
  )
}

// ── EQ ─────────────────────────────────────────────────────────────────────

const EQ_RANGE_DB = 18

type EqNode = {
  key: string
  label: string
  gainId: 'lowGainDb' | 'lowMidGainDb' | 'highMidGainDb' | 'highGainDb'
  freqId?: 'lowMidFreqHz' | 'highMidFreqHz'
  /// Shelf corners are fixed in Rust, so those handles move in gain only.
  fixedHz?: number
}

const EQ_NODES: EqNode[] = [
  { key: 'low', label: 'Low', gainId: 'lowGainDb', fixedHz: LOW_SHELF_HZ },
  { key: 'lowMid', label: 'LM', gainId: 'lowMidGainDb', freqId: 'lowMidFreqHz' },
  { key: 'highMid', label: 'HM', gainId: 'highMidGainDb', freqId: 'highMidFreqHz' },
  { key: 'high', label: 'High', gainId: 'highGainDb', fixedHz: HIGH_SHELF_HZ },
]

/// The summed magnitude of the four real biquads over the input spectrum.
/// Drag a handle to set its gain — and frequency, for the two sweepable mids.
export function EqDisplay({
  params,
  active,
  spectrumRef,
  spectrumLive,
  onChange,
}: {
  params: MixStationParams
  active: boolean
  spectrumRef: RefObject<SpectrumFrame | null>
  spectrumLive: boolean
  onChange: CurveChange
}) {
  return (
    <Plot label="Equaliser response — drag a band handle">
      {({ w, h }) => (
        <>
          <SpectrumOverlay frameRef={spectrumRef} live={spectrumLive} minHz={F_MIN} maxHz={F_MAX} />
          <EqSvg w={w} h={h} params={params} active={active} onChange={onChange} />
        </>
      )}
    </Plot>
  )
}

function EqSvg({ w, h, params, active, onChange }: Box & { params: MixStationParams; active: boolean; onChange: CurveChange }) {
  const y = (db: number) => h / 2 - (clamp(db, -EQ_RANGE_DB, EQ_RANGE_DB) / EQ_RANGE_DB) * (h / 2 - PAD)
  const yDb = (py: number) => ((h / 2 - py) / Math.max(h / 2 - PAD, 1)) * EQ_RANGE_DB
  const { curve, fill } = useMemo(() => {
    const sections = eqSections(params, DISPLAY_SAMPLE_RATE)
    const points: [number, number][] = []
    for (let i = 0; i <= 160; i++) {
      const hz = F_MIN * Math.pow(F_MAX / F_MIN, i / 160)
      points.push([freqX(hz, w), y(chainMagnitudeDb(sections, DISPLAY_SAMPLE_RATE, hz))])
    }
    const line = path(points)
    return { curve: line, fill: `${line}L${w} ${h / 2}L0 ${h / 2}Z` }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [params, w, h])

  return (
    <svg className="relative h-full w-full touch-none" viewBox={`0 0 ${w} ${h}`}>
      <FrequencyGrid w={w} h={h} dbTicks={[12, 6, 0, -6, -12]} y={y} />
      <path d={fill} fill={active ? 'rgb(77 156 248 / 0.12)' : 'transparent'} />
      <path className={`plot-curve ${active ? '' : 'is-idle'}`} d={curve} />
      {EQ_NODES.map((node) => (
        <EqHandle key={node.key} node={node} params={params} w={w} h={h} y={y} yDb={yDb} onChange={onChange} />
      ))}
    </svg>
  )
}

function EqHandle({
  node,
  params,
  w,
  h,
  y,
  yDb,
  onChange,
}: {
  node: EqNode
  params: MixStationParams
  w: number
  h: number
  y: (db: number) => number
  yDb: (py: number) => number
  onChange: CurveChange
}) {
  const drag = useDrag((px, py) => {
    const gain = PARAM_SPECS[node.gainId]
    onChange(node.gainId, clamp(Math.round(yDb(py) * 10) / 10, gain.min, gain.max))
    if (node.freqId) {
      const freq = PARAM_SPECS[node.freqId]
      onChange(node.freqId, clamp(Math.round(xFreq(px, w)), freq.min, freq.max))
    }
  })
  const hz = node.fixedHz ?? params[node.freqId!]
  return (
    <Handle
      bounds={{ w, h }}
      x={freqX(hz, w)}
      y={y(params[node.gainId])}
      label={node.label}
      title={`${node.label} — drag to set ${node.freqId ? 'gain and frequency' : 'gain'}, double-click for 0 dB`}
      drag={drag}
      onReset={() => onChange(node.gainId, 0)}
    />
  )
}

// ── Compressor, limiter ────────────────────────────────────────────────────

function LevelGrid({
  x,
  y,
  floor,
  ceiling,
  ticks,
}: {
  x: (db: number) => number
  y: (db: number) => number
  floor: number
  ceiling: number
  ticks: number[]
}) {
  const [x0, x1, y0, y1] = [x(floor), x(ceiling), y(ceiling), y(floor)]
  return (
    <>
      <rect x={x0} y={y0} width={x1 - x0} height={y1 - y0} fill="rgb(255 255 255 / 0.015)" />
      {ticks.map((db) => (
        <g key={db}>
          <line className="plot-grid" x1={x(db)} x2={x(db)} y1={y0} y2={y1} />
          <line className="plot-grid" x1={x0} x2={x1} y1={y(db)} y2={y(db)} />
          <text className="plot-label" x={x(db)} y={y1 + 11} textAnchor="middle">
            {db}
          </text>
          <text className="plot-label" x={x0 - 5} y={y(db) + 3} textAnchor="end">
            {db}
          </text>
        </g>
      ))}
    </>
  )
}

/// A square level plot inside the measured box, the same dB per pixel on
/// both axes so the unity diagonal is at 45° and a ratio reads as a slope.
function squareAxes(w: number, h: number, floor: number, ceiling: number) {
  const side = Math.max(Math.min(w, h) - PAD * 3, 1)
  const left = (w - side) / 2
  const top = (h - side) / 2 - PAD / 2
  const span = ceiling - floor
  return {
    x: (db: number) => left + ((clamp(db, floor, ceiling) - floor) / span) * side,
    y: (db: number) => top + ((ceiling - clamp(db, floor, ceiling)) / span) * side,
    dbAtX: (px: number) => floor + ((px - left) / side) * span,
    dbAtY: (py: number) => ceiling - ((py - top) / side) * span,
  }
}

/// Static transfer curve with the fixed 6 dB soft knee. The threshold is a
/// handle on the curve you drag sideways.
export function CompressorDisplay({ params, active, onChange }: { params: MixStationParams; active: boolean; onChange: CurveChange }) {
  return (
    <Plot label="Compressor transfer curve — drag the threshold sideways">
      {({ w, h }) => <CompressorSvg w={w} h={h} params={params} active={active} onChange={onChange} />}
    </Plot>
  )
}

function CompressorSvg({ w, h, params, active, onChange }: Box & { params: MixStationParams; active: boolean; onChange: CurveChange }) {
  const axes = squareAxes(w, h, -60, 0)
  const points: [number, number][] = []
  for (let db = -60; db <= 0; db += 0.5) {
    points.push([axes.x(db), axes.y(compressorOutputDb(db, params.compThresholdDb, params.compRatio, params.compMakeupDb))])
  }
  const spec = PARAM_SPECS.compThresholdDb
  const drag = useDrag((px) => onChange('compThresholdDb', clamp(Math.round(axes.dbAtX(px) * 10) / 10, spec.min, spec.max)))
  const at = params.compThresholdDb
  return (
    <svg className="relative h-full w-full touch-none" viewBox={`0 0 ${w} ${h}`}>
      <LevelGrid x={axes.x} y={axes.y} floor={-60} ceiling={0} ticks={[-48, -36, -24, -12]} />
      <line className="plot-reference" x1={axes.x(-60)} y1={axes.y(-60)} x2={axes.x(0)} y2={axes.y(0)} />
      <path className={`plot-curve ${active ? '' : 'is-idle'}`} d={path(points)} />
      <Handle
        bounds={{ w, h }}
        x={axes.x(at)}
        y={axes.y(compressorOutputDb(at, at, params.compRatio, params.compMakeupDb))}
        label={`${at.toFixed(1)} dB`}
        title="Threshold — drag sideways, double-click to reset"
        drag={drag}
        onReset={() => onChange('compThresholdDb', spec.defaultValue)}
      />
    </svg>
  )
}

/// The ceiling the output can never cross, with the soft knee easing into it.
/// Drag the ceiling handle up or down.
export function LimiterDisplay({ params, active, onChange }: { params: MixStationParams; active: boolean; onChange: CurveChange }) {
  return (
    <Plot label="Limiter ceiling — drag the ceiling up or down">
      {({ w, h }) => <LimiterSvg w={w} h={h} params={params} active={active} onChange={onChange} />}
    </Plot>
  )
}

function LimiterSvg({ w, h, params, active, onChange }: Box & { params: MixStationParams; active: boolean; onChange: CurveChange }) {
  const axes = squareAxes(w, h, -24, 0)
  const points: [number, number][] = []
  for (let db = -24; db <= 0; db += 0.25) points.push([axes.x(db), axes.y(limiterOutputDb(db, params.limiterCeilingDb))])
  const spec = PARAM_SPECS.limiterCeilingDb
  const drag = useDrag((_, py) => onChange('limiterCeilingDb', clamp(Math.round(axes.dbAtY(py) * 10) / 10, spec.min, spec.max)))
  const ceiling = params.limiterCeilingDb
  return (
    <svg className="relative h-full w-full touch-none" viewBox={`0 0 ${w} ${h}`}>
      <LevelGrid x={axes.x} y={axes.y} floor={-24} ceiling={0} ticks={[-18, -12, -6]} />
      <line className="plot-reference" x1={axes.x(-24)} y1={axes.y(-24)} x2={axes.x(0)} y2={axes.y(0)} />
      <line
        x1={axes.x(-24)}
        x2={axes.x(0)}
        y1={axes.y(ceiling)}
        y2={axes.y(ceiling)}
        stroke="var(--color-warn)"
        strokeDasharray="4 3"
        vectorEffect="non-scaling-stroke"
      />
      <path className={`plot-curve ${active ? '' : 'is-idle'}`} d={path(points)} />
      <Handle
        bounds={{ w, h }}
        x={axes.x(-2)}
        y={axes.y(ceiling)}
        label={`${ceiling.toFixed(1)} dB`}
        title="Ceiling — drag up or down, double-click to reset"
        drag={drag}
        onReset={() => onChange('limiterCeilingDb', spec.defaultValue)}
      />
    </svg>
  )
}

// ── Drive, width ───────────────────────────────────────────────────────────

/// The saturation transfer curve through the same `saturate` maths as the
/// audio path, so the asymmetry at high Character is the real even-harmonic
/// bias rather than an illustration of it.
export function SaturationDisplay({ params, active }: { params: MixStationParams; active: boolean }) {
  return (
    <Plot label="Saturation transfer curve">
      {({ w, h }) => {
        const side = Math.max(Math.min(w, h) - PAD * 2, 1)
        const left = (w - side) / 2
        const top = (h - side) / 2
        const x = (v: number) => left + ((v + 1.2) / 2.4) * side
        const y = (v: number) => top + ((1.2 - clamp(v, -1.2, 1.2)) / 2.4) * side
        const points: [number, number][] = []
        for (let i = 0; i <= 160; i++) {
          const input = -1.2 + (2.4 * i) / 160
          points.push([x(input), y(saturate(input, params.satDrivePct, params.satCharacterPct))])
        }
        return (
          <svg className="relative h-full w-full" viewBox={`0 0 ${w} ${h}`}>
            <line className="plot-grid is-major" x1={x(0)} x2={x(0)} y1={top} y2={top + side} />
            <line className="plot-grid is-major" x1={left} x2={left + side} y1={y(0)} y2={y(0)} />
            <line className="plot-reference" x1={x(-1.2)} y1={y(-1.2)} x2={x(1.2)} y2={y(1.2)} />
            <path className={`plot-curve ${active ? '' : 'is-idle'}`} d={path(points)} />
            <text className="plot-label" x={left + side} y={top + side + 11} textAnchor="end">
              In → Out
            </text>
          </svg>
        )
      }}
    </Plot>
  )
}

/// Where a hard-panned pair lands after `stereo_width` at this setting. With
/// no image analysis on the bridge this shows the transform, not programme
/// material — and says so.
export function WidthDisplay({ params, active }: { params: MixStationParams; active: boolean }) {
  return (
    <Plot label="Stereo width transform">
      {({ w, h }) => {
        const width = params.widthPct * 0.01
        const [left, right] = stereoWidth(1, -1, width)
        const reach = (w / 2 - 40) * clamp(Math.abs(left), 0, 2) * 0.5
        const cx = w / 2
        const cy = h / 2
        const stroke = active ? 'var(--color-accent)' : 'var(--color-ink-4)'
        const unity = (w / 2 - 40) * 0.5
        return (
          <svg className="relative h-full w-full" viewBox={`0 0 ${w} ${h}`}>
            <line className="plot-grid is-major" x1={cx} x2={cx} y1={PAD} y2={h - PAD} />
            <line className="plot-reference" x1={cx - unity} x2={cx - unity} y1={cy - 24} y2={cy + 24} />
            <line className="plot-reference" x1={cx + unity} x2={cx + unity} y1={cy - 24} y2={cy + 24} />
            <line x1={cx - reach} x2={cx + reach} y1={cy} y2={cy} stroke={stroke} strokeWidth={2.5} strokeLinecap="round" />
            <circle cx={cx - reach} cy={cy} r={6} fill={stroke} />
            <circle cx={cx + reach} cy={cy} r={6} fill={stroke} />
            <text className="plot-label" x={cx - unity} y={cy + 38} textAnchor="middle">
              L
            </text>
            <text className="plot-label" x={cx + unity} y={cy + 38} textAnchor="middle">
              R
            </text>
            <text className="plot-label" x={cx} y={h - 6} textAnchor="middle" style={{ fill: 'var(--color-ink-2)' }}>
              {width === 0 ? 'Mono' : right > 0 ? 'Sides swapped' : width > 1 ? 'Wider than the source' : width < 1 ? 'Narrower than the source' : 'As the source'}
              {' · hard-panned pair, dashed = unchanged'}
            </text>
          </svg>
        )
      }}
    </Plot>
  )
}
