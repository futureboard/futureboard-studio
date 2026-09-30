import { useEffect, useRef, type RefObject } from 'react'
import type { MeterHistory } from './history'
import { clamp } from './math'

const FLOOR_DB = -48
const REDUCTION_SCALE_DB = 24
/// A peak readout holds the loudest of the last this-many frames (~1.2 s at
/// the ~30 Hz telemetry rate).
const HOLD_FRAMES = 36

const levelUnit = (linear: number) =>
  linear > 0 ? clamp((20 * Math.log10(linear) - FLOOR_DB) / -FLOOR_DB, 0, 1) : 0

type Key = 'in' | 'gr' | 'out'
type Column = {
  fill: HTMLDivElement | null
  peak: HTMLDivElement | null
  text: HTMLSpanElement | null
}

/**
 * In, reduction and out as three vertical bars with held peak readouts.
 *
 * Read from the same history ring the stage draws, so the hold sees every
 * frame the host sent — sampling the latest frame on animation frames would
 * miss peaks whenever the page paints slower than the telemetry arrives.
 * Written straight to style and text, so telemetry never re-renders the
 * editor. With no frame the bars read empty and the numbers `—`.
 */
export function Meters({
  historyRef,
  reductionLabel,
  reductionSign = '−',
  active,
}: {
  historyRef: RefObject<MeterHistory>
  reductionLabel: string
  /// Prefix of the reduction readout: a minus for gain taken off, nothing for
  /// a shaper whose reading is the size of a boost or a cut.
  reductionSign?: string
  active: boolean
}) {
  const columns = useRef<Record<Key, Column>>({
    in: { fill: null, peak: null, text: null },
    gr: { fill: null, peak: null, text: null },
    out: { fill: null, peak: null, text: null },
  })
  const activeRef = useRef(active)
  useEffect(() => {
    activeRef.current = active
  }, [active])

  useEffect(() => {
    let raf = 0
    let drawnStamp = -1
    const paint = () => {
      raf = requestAnimationFrame(paint)
      const history = historyRef.current
      if (history.stamp === drawnStamp) return
      drawnStamp = history.stamp
      const c = columns.current
      const text = (key: Key, value: string) => {
        const node = c[key].text
        if (node && node.textContent !== value) node.textContent = value
      }
      if (history.count === 0) {
        for (const key of ['in', 'gr', 'out'] as const) {
          if (c[key].fill) c[key].fill!.style.transform = 'scaleY(0)'
          if (c[key].peak) c[key].peak!.style.opacity = '0'
          text(key, '—')
        }
        return
      }
      const newest = history.indexFromNewest(0)
      const grUnit = (db: number) => (activeRef.current ? clamp(db / REDUCTION_SCALE_DB, 0, 1) : 0)
      const now = {
        in: levelUnit(history.inPeak[newest]!),
        out: levelUnit(history.outPeak[newest]!),
        gr: grUnit(history.reduction[newest]!),
      }
      const held = { in: 0, out: 0, gr: 0 }
      for (let age = 0; age < Math.min(HOLD_FRAMES, history.count); age++) {
        const index = history.indexFromNewest(age)
        held.in = Math.max(held.in, levelUnit(history.inPeak[index]!))
        held.out = Math.max(held.out, levelUnit(history.outPeak[index]!))
        held.gr = Math.max(held.gr, grUnit(history.reduction[index]!))
      }
      for (const key of ['in', 'gr', 'out'] as const) {
        const column = c[key]
        if (column.fill) column.fill.style.transform = `scaleY(${now[key]})`
        if (column.peak) {
          column.peak.style[key === 'gr' ? 'top' : 'bottom'] = `${held[key] * 100}%`
          column.peak.style.opacity = held[key] > 0.001 ? '1' : '0'
        }
      }
      const levelText = (unit: number) => (unit <= 0 ? '−∞' : (FLOOR_DB - unit * FLOOR_DB).toFixed(1))
      text('in', levelText(held.in))
      text('out', levelText(held.out))
      const gr = held.gr * REDUCTION_SCALE_DB
      text('gr', gr >= 0.05 ? `${reductionSign}${gr.toFixed(1)}` : '0.0')
    }
    raf = requestAnimationFrame(paint)
    return () => cancelAnimationFrame(raf)
  }, [historyRef, reductionSign])

  const bind = (key: Key) => ({
    fill: (node: HTMLDivElement | null) => {
      columns.current[key].fill = node
    },
    peak: (node: HTMLDivElement | null) => {
      columns.current[key].peak = node
    },
    text: (node: HTMLSpanElement | null) => {
      columns.current[key].text = node
    },
  })

  return (
    <div className="flex h-full min-h-0 justify-around gap-2" aria-label="Meters">
      <MeterColumn label="In" refs={bind('in')} tone="level" />
      <MeterColumn label={reductionLabel} refs={bind('gr')} tone="reduction" />
      <MeterColumn label="Out" refs={bind('out')} tone="level" />
    </div>
  )
}

function MeterColumn({
  label,
  refs,
  tone,
}: {
  label: string
  refs: {
    fill: (node: HTMLDivElement | null) => void
    peak: (node: HTMLDivElement | null) => void
    text: (node: HTMLSpanElement | null) => void
  }
  tone: 'level' | 'reduction'
}) {
  const reduction = tone === 'reduction'
  return (
    <div className="flex min-h-0 w-12 flex-col items-center gap-1.5">
      <span className="cap">{label}</span>
      <div className="relative min-h-0 w-3 flex-1 overflow-hidden bg-canvas" aria-hidden="true">
        <div
          ref={refs.fill}
          className={`absolute inset-x-0 h-full ${
            reduction ? 'top-0 origin-top bg-accent' : 'bottom-0 origin-bottom bg-ink-3'
          }`}
          style={{ transform: 'scaleY(0)' }}
        />
        <div
          ref={refs.peak}
          className={`absolute inset-x-0 h-px ${reduction ? 'bg-accent-hi' : 'bg-ink'}`}
          style={{ opacity: 0 }}
        />
      </div>
      <span ref={refs.text} className="num text-[11px] font-semibold text-ink-2">
        —
      </span>
    </div>
  )
}
