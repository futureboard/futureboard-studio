import { useEffect, useRef, useState, type CSSProperties, type PointerEvent as ReactPointerEvent } from 'react'
import {
  connectBridge,
  defaultParams,
  padParamId,
  postBrowseSample,
  postParam,
  type DrumSamplerParams,
  type Pad,
  type SampleLoadResult,
  type WireField,
} from './bridge'

const NOTE_NAMES = ['C', 'C#', 'D', 'D#', 'E', 'F', 'F#', 'G', 'G#', 'A', 'A#', 'B']

function midiNoteName(note: number): string {
  const octave = Math.floor(note / 12) - 1
  return `${NOTE_NAMES[note % 12]}${octave}`
}

// Choke groups get a small, distinct tag color each — never the accent cyan,
// which stays reserved for selection/focus. Two-channel: the swatch color
// pairs with a group number printed inside it, so color blindness never
// hides which pads are linked.
const CHOKE_COLORS = ['#e0925a', '#d8c15a', '#8fca6a', '#6ac0a8', '#6aa8ca', '#8a8fd6', '#b07fd0', '#d67fae']

type KnobProps = {
  label: string
  value: number
  min: number
  max: number
  step?: number
  display?: (value: number) => string
  onChange: (value: number) => void
}

function Knob({ label, value, min, max, step = 0.01, display, onChange }: KnobProps) {
  const ratio = (value - min) / (max - min)
  const angle = -135 + ratio * 270
  return (
    <label className="knob-control">
      <span className="knob-label">{label}</span>
      <span className="knob" style={{ '--knob-angle': `${angle}deg` } as CSSProperties}>
        <span className="knob-cap">
          <i />
        </span>
        <input
          aria-label={label}
          type="range"
          min={min}
          max={max}
          step={step}
          value={value}
          onChange={(event) => onChange(Number(event.target.value))}
        />
      </span>
      <output>{display ? display(value) : value.toFixed(step < 0.1 ? 2 : 0)}</output>
    </label>
  )
}

type PadCellProps = {
  index: number
  pad: Pad
  selected: boolean
  loading: boolean
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

// Quick-tweak gestures live on the pad itself so a level or pitch nudge never
// needs the inspector open. A button already fires `click` for both mouse and
// keyboard (Enter/Space) activation, so selection stays on `onClick` for free
// keyboard access; the drag path only has to detect a real drag and swallow
// the click that follows it, never the other way around.
function PadCell({ index, pad, selected, loading, onSelect, onAdjust }: PadCellProps) {
  const empty = !pad.sampleName
  const dragRef = useRef<PadDrag | null>(null)
  const suppressClickRef = useRef(false)
  const [adjustField, setAdjustField] = useState<'gain' | 'tune' | null>(null)

  const handlePointerDown = (event: ReactPointerEvent<HTMLButtonElement>) => {
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

  const handlePointerMove = (event: ReactPointerEvent<HTMLButtonElement>) => {
    const drag = dragRef.current
    if (!drag || drag.pointerId !== event.pointerId) return
    const deltaY = drag.startY - event.clientY
    if (!drag.dragging) {
      if (Math.abs(deltaY) < 4) return
      drag.dragging = true
      setAdjustField(drag.field)
    }
    const [min, max, scale] = drag.field === 'gain' ? [-60, 12, 0.25] : [-24, 24, 0.15]
    onAdjust(drag.field, Math.min(max, Math.max(min, drag.startValue + deltaY * scale)))
  }

  const endDrag = (event: ReactPointerEvent<HTMLButtonElement>) => {
    const drag = dragRef.current
    if (!drag || drag.pointerId !== event.pointerId) return
    dragRef.current = null
    setAdjustField(null)
    if (drag.dragging) suppressClickRef.current = true
  }

  const handleClick = () => {
    if (suppressClickRef.current) {
      suppressClickRef.current = false
      return
    }
    onSelect()
  }

  const handleDoubleClick = () => {
    if (empty) return
    onAdjust('gain', 0)
    onAdjust('tune', 0)
  }

  const classes = ['pad-cell']
  if (selected) classes.push('selected')
  if (empty) classes.push('empty')
  if (loading) classes.push('loading')
  return (
    <button
      type="button"
      className={classes.join(' ')}
      onClick={handleClick}
      onDoubleClick={handleDoubleClick}
      onPointerDown={handlePointerDown}
      onPointerMove={handlePointerMove}
      onPointerUp={endDrag}
      onPointerCancel={endDrag}
      aria-pressed={selected}
      title={empty ? undefined : 'Drag: gain · Shift-drag: tune · Double-click: reset'}
    >
      <span className="pad-index">{String(index + 1).padStart(2, '0')}</span>
      <span className="pad-name">{loading ? 'Loading…' : (pad.sampleName ?? 'Empty')}</span>
      <span className="pad-tags">
        {pad.choke > 0 && (
          <i
            className="pad-tag choke"
            style={{ background: CHOKE_COLORS[(pad.choke - 1) % CHOKE_COLORS.length] }}
            title={`Choke group ${pad.choke}`}
          >
            {pad.choke}
          </i>
        )}
        {pad.mute && <i className="pad-tag mute" title="Muted">M</i>}
        {pad.solo && <i className="pad-tag solo" title="Solo">S</i>}
      </span>
      {adjustField && (
        <span className="pad-adjust">
          <b>{adjustField === 'gain' ? 'Gain' : 'Tune'}</b>
          {adjustField === 'gain'
            ? `${pad.gain > 0 ? '+' : ''}${pad.gain.toFixed(1)}dB`
            : `${pad.tune > 0 ? '+' : ''}${pad.tune}st`}
        </span>
      )}
    </button>
  )
}

function App() {
  const [params, setParams] = useState<DrumSamplerParams>(defaultParams)
  const [connected, setConnected] = useState(false)
  const [selected, setSelected] = useState(0)
  const [loadingPad, setLoadingPad] = useState<number | null>(null)
  const [lastError, setLastError] = useState<string | null>(null)

  useEffect(
    () =>
      connectBridge(
        setParams,
        setConnected,
        (result: SampleLoadResult) => {
          setLoadingPad((current) => (current === result.padIndex ? null : current))
          if (result.ok) {
            setLastError(null)
            setParams((current) => {
              const pads = current.pads.slice()
              pads[result.padIndex] = { ...pads[result.padIndex], sampleName: result.name }
              return { pads }
            })
          } else {
            setLastError(`${result.name}: ${result.error ?? 'load failed'}`)
          }
        },
      ),
    [],
  )

  const pad = params.pads[selected]

  const setPadField = <K extends WireField>(padIndex: number, field: K, value: Pad[K], wire: number) => {
    setParams((current) => {
      const pads = current.pads.slice()
      pads[padIndex] = { ...pads[padIndex], [field]: value }
      return { pads }
    })
    postParam(padParamId(padIndex, field), wire)
  }

  const changePad = <K extends WireField>(field: K, value: Pad[K], wire: number) =>
    setPadField(selected, field, value, wire)

  // Same wire path as the Voice module's own knobs — a pad-drag nudge and an
  // inspector-knob nudge are indistinguishable to the engine.
  const adjustPad = (padIndex: number, field: 'gain' | 'tune', value: number) =>
    setPadField(padIndex, field, value, value)

  const browse = () => {
    setLoadingPad(selected)
    setLastError(null)
    postBrowseSample(selected)
  }

  const loadedCount = params.pads.filter((entry) => entry.sampleName).length

  return (
    <main>
      <div className="instrument-shell">
        <header className="topbar">
          <div className="brand">
            <span>D</span>
            <div>
              <strong>Drum Sampler</strong>
              <small>16-pad one-shot kit</small>
            </div>
          </div>
          <div className="status">
            <i className={connected ? 'online' : ''} />
            <span>{connected ? 'Connected' : 'Preview'}</span>
          </div>
        </header>

        <div className="workspace">
          <section className="module pad-panel">
            <header>
              <strong>Pads</strong>
              <small>4 × 4 kit</small>
              <span className="pad-count">{loadedCount} / 16</span>
            </header>
            <div className="pad-grid" role="group" aria-label="Pads">
              {params.pads.map((entry, index) => (
                <PadCell
                  key={index}
                  index={index}
                  pad={entry}
                  selected={index === selected}
                  loading={loadingPad === index}
                  onSelect={() => setSelected(index)}
                  onAdjust={(field, value) => adjustPad(index, field, value)}
                />
              ))}
            </div>
          </section>

          <aside className="inspector">
            <div className="lcd">
              <span className="lcd-label">Pad {String(selected + 1).padStart(2, '0')}</span>
              <strong className="lcd-value">{pad.sampleName ?? 'NO SAMPLE'}</strong>
              {lastError && <span className="lcd-error">{lastError}</span>}
            </div>

            <button type="button" className="browse" onClick={browse} disabled={loadingPad === selected}>
              {loadingPad === selected ? 'Loading…' : 'Browse…'}
            </button>

            <div className="module">
              <header>
                <strong>Voice</strong>
                <small>Pitch &amp; level</small>
              </header>
              <div className="knob-row">
                <Knob label="TUNE" value={pad.tune} min={-24} max={24} step={1} display={(v) => `${v > 0 ? '+' : ''}${v}st`} onChange={(v) => changePad('tune', v, v)} />
                <Knob label="GAIN" value={pad.gain} min={-60} max={12} step={0.1} display={(v) => `${v.toFixed(1)}dB`} onChange={(v) => changePad('gain', v, v)} />
                <Knob label="PAN" value={pad.pan} min={-1} max={1} step={0.01} display={(v) => (v === 0 ? 'C' : v < 0 ? `L${Math.round(-v * 100)}` : `R${Math.round(v * 100)}`)} onChange={(v) => changePad('pan', v, v)} />
              </div>
            </div>

            <div className="module">
              <header>
                <strong>Envelope</strong>
                <small>Attack &amp; release</small>
              </header>
              <div className="knob-row">
                <Knob label="ATTACK" value={pad.attack} min={0} max={250} step={1} display={(v) => `${Math.round(v)}ms`} onChange={(v) => changePad('attack', v, v)} />
                <Knob label="RELEASE" value={pad.release} min={1} max={2000} step={1} display={(v) => `${Math.round(v)}ms`} onChange={(v) => changePad('release', v, v)} />
              </div>
            </div>

            <div className="module">
              <header>
                <strong>Trigger</strong>
                <small>Note, choke &amp; mix</small>
              </header>
              <div className="stepper-row">
                <label className="stepper">
                  <span>Note</span>
                  <div className="stepper-control">
                    <button type="button" onClick={() => changePad('note', Math.max(0, pad.note - 1), Math.max(0, pad.note - 1))}>−</button>
                    <output>{midiNoteName(pad.note)}</output>
                    <button type="button" onClick={() => changePad('note', Math.min(127, pad.note + 1), Math.min(127, pad.note + 1))}>+</button>
                  </div>
                </label>
                <label className="stepper">
                  <span>Choke</span>
                  <div className="stepper-control">
                    <button type="button" onClick={() => changePad('choke', Math.max(0, pad.choke - 1), Math.max(0, pad.choke - 1))}>−</button>
                    <output>
                      {pad.choke > 0 && (
                        <i className="choke-swatch" style={{ background: CHOKE_COLORS[(pad.choke - 1) % CHOKE_COLORS.length] }} />
                      )}
                      {pad.choke === 0 ? 'Off' : pad.choke}
                    </output>
                    <button type="button" onClick={() => changePad('choke', Math.min(8, pad.choke + 1), Math.min(8, pad.choke + 1))}>+</button>
                  </div>
                </label>
              </div>
              <div className="toggle-row">
                <button type="button" className={pad.reverse ? 'active' : ''} aria-pressed={pad.reverse} onClick={() => changePad('reverse', !pad.reverse, pad.reverse ? 0 : 1)}>
                  Reverse
                </button>
                <button type="button" className={pad.mute ? 'active mute' : ''} aria-pressed={pad.mute} onClick={() => changePad('mute', !pad.mute, pad.mute ? 0 : 1)}>
                  Mute
                </button>
                <button type="button" className={pad.solo ? 'active solo' : ''} aria-pressed={pad.solo} onClick={() => changePad('solo', !pad.solo, pad.solo ? 0 : 1)}>
                  Solo
                </button>
              </div>
            </div>
          </aside>
        </div>

        <footer>
          <span>16 pads · one-shot</span>
          <span>MIDI follows track input</span>
        </footer>
      </div>
    </main>
  )
}

export default App
