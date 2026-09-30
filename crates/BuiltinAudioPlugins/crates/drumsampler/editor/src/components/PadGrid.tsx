import { useRef, useState, type PointerEvent as ReactPointerEvent } from 'react'
import type { SampleInfo } from '../bridge'
import { RANGE, effectiveRegion, formatDb, formatSemis, noteName, type Pad } from '../lib/pads'

type PadCellProps = {
  index: number
  pad: Pad
  sample: SampleInfo | null
  level: number
  selected: boolean
  loading: boolean
  error: boolean
  dropTarget: boolean
  onSelect: () => void
  onAdjust: (field: 'gain' | 'tune', value: number) => void
}

type PadDrag = {
  pointerId: number
  field: 'gain' | 'tune'
  startY: number
  startValue: number
  dragging: boolean
}

/// The pad's sample as a small mirrored silhouette, the played region bright
/// and the trimmed-off ends dim.
function MiniWave({ peaks, pad }: { peaks: number[]; pad: Pad }) {
  if (peaks.length < 2) return <span className="mini-wave is-empty" />
  const [lo, hi] = effectiveRegion(pad)
  const last = peaks.length - 1
  const top = peaks.map((p, i) => `${(i / last) * 100},${50 - (p / 255) * 46}`).join(' ')
  const bottom = peaks
    .map((p, i) => `${((last - i) / last) * 100},${50 + (peaks[last - i]! / 255) * 46}`)
    .join(' ')
  return (
    <svg className="mini-wave" viewBox="0 0 100 100" preserveAspectRatio="none" aria-hidden="true">
      <polygon points={`${top} ${bottom}`} className="mini-wave-shape" />
      <rect x={0} y={0} width={lo * 100} height={100} className="mini-wave-trim" />
      <rect x={hi * 100} y={0} width={(1 - hi) * 100} height={100} className="mini-wave-trim" />
      {pad.reverse && <path d="M 96 8 L 88 14 L 96 20" className="mini-wave-reverse" />}
    </svg>
  )
}

/// One pad. Click selects; dragging nudges the level (Shift: the pitch), so a
/// quick tweak never needs the inspector; double-click resets both. A button
/// already fires `click` for mouse and keyboard, so selection stays on
/// `onClick` and the drag path only has to swallow the click that ends a real
/// drag.
function PadCell({ index, pad, sample, level, selected, loading, error, dropTarget, onSelect, onAdjust }: PadCellProps) {
  const empty = !pad.sampleName
  const dragRef = useRef<PadDrag | null>(null)
  const suppressClickRef = useRef(false)
  const [adjustField, setAdjustField] = useState<'gain' | 'tune' | null>(null)

  const onPointerDown = (event: ReactPointerEvent<HTMLButtonElement>) => {
    if (event.button !== 0 || empty) return
    const field = event.shiftKey ? 'tune' : 'gain'
    dragRef.current = {
      pointerId: event.pointerId,
      field,
      startY: event.clientY,
      startValue: field === 'gain' ? pad.gain : pad.tune,
      dragging: false,
    }
    event.currentTarget.setPointerCapture(event.pointerId)
  }

  const onPointerMove = (event: ReactPointerEvent<HTMLButtonElement>) => {
    const drag = dragRef.current
    if (!drag || drag.pointerId !== event.pointerId) return
    const deltaY = drag.startY - event.clientY
    if (!drag.dragging) {
      if (Math.abs(deltaY) < 4) return
      drag.dragging = true
      setAdjustField(drag.field)
    }
    const [min, max] = RANGE[drag.field]
    const scale = drag.field === 'gain' ? 0.25 : 0.1
    const raw = drag.startValue + deltaY * scale
    const stepped = drag.field === 'tune' ? Math.round(raw) : Math.round(raw * 10) / 10
    onAdjust(drag.field, Math.min(max, Math.max(min, stepped)))
  }

  const endDrag = (event: ReactPointerEvent<HTMLButtonElement>) => {
    const drag = dragRef.current
    if (!drag || drag.pointerId !== event.pointerId) return
    dragRef.current = null
    setAdjustField(null)
    if (drag.dragging) suppressClickRef.current = true
  }

  const onClick = () => {
    if (suppressClickRef.current) {
      suppressClickRef.current = false
      return
    }
    onSelect()
  }

  const meter = Math.min(1, Math.max(0, level > 0 ? 1 + Math.log10(level) / 3 : 0))
  const classes = ['pad']
  if (selected) classes.push('is-selected')
  if (empty) classes.push('is-empty')
  if (loading) classes.push('is-loading')
  if (error) classes.push('is-error')
  if (dropTarget) classes.push('is-drop-target')
  if (meter > 0.02) classes.push('is-playing')

  return (
    <button
      type="button"
      className={classes.join(' ')}
      data-pad-index={index}
      onClick={onClick}
      onDoubleClick={() => {
        if (empty) return
        onAdjust('gain', 0)
        onAdjust('tune', 0)
      }}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={endDrag}
      onPointerCancel={endDrag}
      aria-pressed={selected}
      aria-label={`Pad ${index + 1}, ${noteName(pad.note)}, ${pad.sampleName ?? 'empty'}`}
      title={
        empty
          ? 'Empty — drop an audio file here, or select it and load one'
          : 'Drag: gain · Shift-drag: tune · Double-click: reset · Drop a file to replace'
      }
    >
      <span className="pad-top">
        <span className="pad-index num">{String(index + 1).padStart(2, '0')}</span>
        <span className="pad-tags">
          {pad.choke > 0 && (
            <i className="tag" title={`Choke group ${pad.choke}`}>
              C{pad.choke}
            </i>
          )}
          {pad.filterMode !== 'off' && <i className="tag" title="Filter on">F</i>}
          {pad.mute && <i className="tag tag-mute" title="Muted">M</i>}
          {pad.solo && <i className="tag tag-solo" title="Solo">S</i>}
        </span>
        <span className="pad-note num">{noteName(pad.note)}</span>
      </span>
      <MiniWave peaks={sample?.peaks ?? []} pad={pad} />
      <span className="pad-name">{loading ? 'Loading…' : (pad.sampleName ?? 'Empty')}</span>
      <span className="pad-meter" aria-hidden="true">
        <span style={{ width: `${meter * 100}%` }} />
      </span>
      {adjustField && (
        <span className="pad-adjust num">
          <b>{adjustField === 'gain' ? 'Gain' : 'Tune'}</b>
          {adjustField === 'gain' ? `${formatDb(pad.gain)} dB` : `${formatSemis(pad.tune)} st`}
        </span>
      )}
    </button>
  )
}

export function PadGrid({
  pads,
  samples,
  levels,
  selected,
  loading,
  errors,
  dropTargets,
  onSelect,
  onAdjust,
}: {
  pads: Pad[]
  samples: (SampleInfo | null)[]
  levels: number[]
  selected: number
  loading: Set<number>
  errors: Map<number, string>
  dropTargets: readonly number[]
  onSelect: (index: number) => void
  onAdjust: (index: number, field: 'gain' | 'tune', value: number) => void
}) {
  // Row 1 at the bottom, like a hardware pad controller: pad 1 (C1, the kick
  // by General MIDI convention) sits bottom-left.
  const order = [12, 13, 14, 15, 8, 9, 10, 11, 4, 5, 6, 7, 0, 1, 2, 3]
  return (
    <div className="pad-grid" role="group" aria-label="Pads">
      {order.map((index) => (
        <PadCell
          key={index}
          index={index}
          pad={pads[index]!}
          sample={samples[index] ?? null}
          level={levels[index] ?? 0}
          selected={index === selected}
          loading={loading.has(index)}
          error={errors.has(index)}
          dropTarget={dropTargets.includes(index)}
          onSelect={() => onSelect(index)}
          onAdjust={(field, value) => onAdjust(index, field, value)}
        />
      ))}
    </div>
  )
}
