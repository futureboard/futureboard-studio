import { useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from 'react'
import type { SampleInfo } from '../bridge'
import {
  MIN_REGION,
  clamp,
  effectiveRegion,
  envelopeAt,
  formatMs,
  regionSeconds,
  type Pad,
} from '../lib/pads'

const HANDLE_HIT_PX = 10

type Drag = { edge: 'start' | 'end' } | null

/// The selected pad's sample: its waveform, the region it plays (drag the two
/// edges), and the envelope the DSP lays over that region drawn on top, in
/// time, at the pad's tune. Double-click an edge to reset it.
export function WaveformEditor({
  pad,
  sample,
  masterTune,
  onRegion,
}: {
  pad: Pad
  sample: SampleInfo | null
  masterTune: number
  onRegion: (edge: 'start' | 'end', value: number) => void
}) {
  const hostRef = useRef<HTMLDivElement>(null)
  const [size, setSize] = useState({ w: 0, h: 0 })
  const [drag, setDrag] = useState<Drag>(null)

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
  const peaks = sample?.peaks ?? []
  const [lo, hi] = effectiveRegion(pad)
  const mid = h / 2
  const amp = Math.max(h / 2 - 10, 1)
  const x = (fraction: number) => fraction * w

  const wavePath = (() => {
    if (peaks.length < 2 || w <= 0) return ''
    const last = peaks.length - 1
    const top = peaks.map((p, i) => `${x(i / last).toFixed(1)},${(mid - (p / 255) * amp).toFixed(1)}`)
    const bottom = peaks
      .map((_, i) => last - i)
      .map((i) => `${x(i / last).toFixed(1)},${(mid + (peaks[i]! / 255) * amp).toFixed(1)}`)
    return `M ${top.join(' L ')} L ${bottom.join(' L ')} Z`
  })()

  // Envelope across the region, in time: the region plays for `seconds`, so
  // sample the AHD curve along it. Reverse plays the same region backwards
  // but the envelope still runs from the trigger, so it is drawn from the
  // edge playback starts at.
  const seconds = sample ? regionSeconds(pad, sample.frames, sample.sampleRate, masterTune) : 0
  const envelopePath = (() => {
    if (seconds <= 0 || w <= 0) return ''
    const points: string[] = []
    const steps = Math.max(Math.round((hi - lo) * w), 2)
    for (let i = 0; i <= steps; i++) {
      const t = (i / steps) * seconds
      const fraction = pad.reverse ? hi - (i / steps) * (hi - lo) : lo + (i / steps) * (hi - lo)
      const level = envelopeAt(pad, t)
      points.push(`${x(fraction).toFixed(1)},${(mid - level * amp).toFixed(1)}`)
    }
    return `M ${points.join(' L ')}`
  })()

  const fractionAt = (clientX: number) => {
    const rect = hostRef.current?.getBoundingClientRect()
    if (!rect || rect.width <= 0) return 0
    return clamp((clientX - rect.left) / rect.width, 0, 1)
  }

  const onPointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!sample || event.button !== 0) return
    const px = fractionAt(event.clientX) * w
    const nearStart = Math.abs(px - x(lo))
    const nearEnd = Math.abs(px - x(hi))
    const edge = nearStart <= nearEnd ? 'start' : 'end'
    if (Math.min(nearStart, nearEnd) > HANDLE_HIT_PX) return
    event.preventDefault()
    event.currentTarget.setPointerCapture(event.pointerId)
    setDrag({ edge })
  }

  const onPointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!drag) return
    const f = fractionAt(event.clientX)
    if (drag.edge === 'start') onRegion('start', clamp(f, 0, hi - MIN_REGION))
    else onRegion('end', clamp(f, lo + MIN_REGION, 1))
  }

  const endDrag = (event: ReactPointerEvent<HTMLDivElement>) => {
    setDrag(null)
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId)
    }
  }

  const onDoubleClick = (event: React.MouseEvent<HTMLDivElement>) => {
    const px = fractionAt(event.clientX) * w
    if (Math.abs(px - x(lo)) <= HANDLE_HIT_PX) onRegion('start', 0)
    else if (Math.abs(px - x(hi)) <= HANDLE_HIT_PX) onRegion('end', 1)
  }

  const keyStep = (event: React.KeyboardEvent, edge: 'start' | 'end') => {
    const delta = event.shiftKey ? 0.001 : 0.01
    const current = edge === 'start' ? lo : hi
    if (event.key === 'ArrowRight' || event.key === 'ArrowUp') {
      event.preventDefault()
      onRegion(edge, edge === 'start' ? clamp(current + delta, 0, hi - MIN_REGION) : clamp(current + delta, lo + MIN_REGION, 1))
    } else if (event.key === 'ArrowLeft' || event.key === 'ArrowDown') {
      event.preventDefault()
      onRegion(edge, edge === 'start' ? clamp(current - delta, 0, hi - MIN_REGION) : clamp(current - delta, lo + MIN_REGION, 1))
    }
  }

  const fullSeconds = sample && sample.sampleRate > 0 ? sample.frames / sample.sampleRate : 0

  return (
    <div className="wave-editor">
      <div
        ref={hostRef}
        className={`wave-stage ${sample ? '' : 'is-empty'} ${drag ? 'is-dragging' : ''}`}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={endDrag}
        onPointerCancel={endDrag}
        onDoubleClick={onDoubleClick}
      >
        {w > 0 && h > 0 && (
          <svg width={w} height={h} viewBox={`0 0 ${w} ${h}`} aria-hidden="true">
            <line className="wave-axis" x1={0} x2={w} y1={mid} y2={mid} />
            {wavePath && <path className="wave-shape" d={wavePath} />}
            {sample && (
              <>
                <rect className="wave-trim" x={0} y={0} width={x(lo)} height={h} />
                <rect className="wave-trim" x={x(hi)} y={0} width={w - x(hi)} height={h} />
                {envelopePath && <path className="wave-envelope" d={envelopePath} />}
                <line className="wave-edge" x1={x(lo)} x2={x(lo)} y1={0} y2={h} />
                <line className="wave-edge" x1={x(hi)} x2={x(hi)} y1={0} y2={h} />
                <rect className="wave-grip" x={x(lo)} y={0} width={8} height={16} rx={2} />
                <rect className="wave-grip" x={x(hi) - 8} y={0} width={8} height={16} rx={2} />
              </>
            )}
          </svg>
        )}
        {!sample && (
          <p className="wave-placeholder">
            {pad.sampleName ? `${pad.sampleName} — loading…` : 'No sample on this pad — drop an audio file here, or pick one from the library'}
          </p>
        )}
        {sample && (
          <>
            <span
              className="wave-handle-focus"
              role="slider"
              tabIndex={0}
              aria-label="Sample start"
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={Math.round(lo * 1000) / 10}
              onKeyDown={(event) => keyStep(event, 'start')}
              style={{ left: x(lo) }}
            />
            <span
              className="wave-handle-focus"
              role="slider"
              tabIndex={0}
              aria-label="Sample end"
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={Math.round(hi * 1000) / 10}
              onKeyDown={(event) => keyStep(event, 'end')}
              style={{ left: x(hi) }}
            />
          </>
        )}
      </div>
      <div className="wave-footer num">
        <span>
          Start <b>{(lo * 100).toFixed(1)}%</b>
        </span>
        <span>
          End <b>{(hi * 100).toFixed(1)}%</b>
        </span>
        <span>
          Plays <b>{seconds > 0 ? formatMs(seconds * 1000) : '—'}</b>
        </span>
        <span className="wave-meta">
          {sample
            ? `${formatMs(fullSeconds * 1000)} · ${sample.channels === 1 ? 'mono' : 'stereo'} · ${(sample.sampleRate / 1000).toFixed(1)} kHz`
            : ''}
        </span>
      </div>
    </div>
  )
}
