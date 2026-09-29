import { useCallback, useEffect, useRef, useState } from 'react'
import {
  connectBridge,
  postBrowseSample,
  postListSamples,
  postLoadSample,
  postParam,
  postPlaceDroppedFiles,
  type FileDrop,
  type SampleFile,
  type SampleInfo,
  type SampleLoadResult,
} from './bridge'
import {
  FILTER_LABELS,
  FILTER_MODES,
  PAD_COUNT,
  RANGE,
  defaultKit,
  dropPads,
  formatDb,
  formatHz,
  formatMs,
  formatPan,
  formatPercent,
  formatSemis,
  noteName,
  padParamId,
  wireValue,
  type FilterMode,
  type Kit,
  type Pad,
  type PadField,
} from './lib/pads'
import { Module, Segmented, Stepper, Toggle } from './components/Controls'
import { FilterCurve } from './components/FilterCurve'
import { Knob, logTravel, timeTravel } from './components/Knob'
import { PadGrid } from './components/PadGrid'
import { SampleLibrary } from './components/SampleLibrary'
import { WaveformEditor } from './components/WaveformEditor'

const EMPTY_LEVELS: number[] = Array.from({ length: PAD_COUNT }, () => 0)
const cutoffTravel = logTravel(RANGE.cutoff[0], RANGE.cutoff[1])
const holdTravel = timeTravel(1, RANGE.hold[1])
const decayTravel = timeTravel(10, RANGE.decay[1])
const attackTravel = timeTravel(0.5, RANGE.attack[1])

/// Where a file drag is over: a pad, the library (import only), or anywhere
/// else, which means the selected pad.
type DropZone = { kind: 'pad'; index: number } | { kind: 'library' } | { kind: 'selected' }

function dropZoneAt(target: EventTarget | null): DropZone {
  const element = target instanceof Element ? target : null
  const pad = element?.closest<HTMLElement>('[data-pad-index]')
  if (pad) return { kind: 'pad', index: Number(pad.dataset.padIndex) }
  if (element?.closest('[data-drop="library"]')) return { kind: 'library' }
  return { kind: 'selected' }
}

const dropZoneAtPoint = (x: number, y: number) => dropZoneAt(document.elementFromPoint(x, y))

const sameZone = (a: DropZone, b: DropZone) =>
  a.kind === b.kind && (a.kind !== 'pad' || (b.kind === 'pad' && a.index === b.index))

function padRange(pads: number[]) {
  const label = (index: number) => String(index + 1).padStart(2, '0')
  if (pads.length === 1) return `Pad ${label(pads[0]!)}`
  return `Pads ${label(pads[0]!)}–${label(pads[pads.length - 1]!)}`
}

function levelToUnit(linear: number) {
  if (!(linear > 0)) return 0
  return Math.max(0, Math.min(1, (20 * Math.log10(linear) + 60) / 60))
}

function App() {
  const [kit, setKit] = useState<Kit>(defaultKit)
  const [connected, setConnected] = useState(false)
  const [selected, setSelected] = useState(0)
  const [samples, setSamples] = useState<(SampleInfo | null)[]>(() => Array(PAD_COUNT).fill(null))
  const [loading, setLoading] = useState<Set<number>>(() => new Set())
  const [errors, setErrors] = useState<Map<number, string>>(() => new Map())
  const [levels, setLevels] = useState<number[]>(EMPTY_LEVELS)
  const [output, setOutput] = useState({ peak: 0, rms: 0 })
  const [files, setFiles] = useState<SampleFile[] | null>(null)
  const [drag, setDrag] = useState<{ zone: DropZone; count: number } | null>(null)
  const [dropNote, setDropNote] = useState<string | null>(null)

  useEffect(
    () =>
      connectBridge({
        onKit: (next) => {
          setKit(next)
          // A different instance: its waveforms arrive as load results.
          setSamples(Array(PAD_COUNT).fill(null))
          setErrors(new Map())
          setLoading(new Set())
        },
        onConnection: (isConnected) => {
          setConnected(isConnected)
          if (isConnected) postListSamples()
          else {
            setLevels(EMPTY_LEVELS)
            setOutput({ peak: 0, rms: 0 })
          }
        },
        onSampleResult: (result: SampleLoadResult) => {
          setLoading((current) => {
            const next = new Set(current)
            next.delete(result.padIndex)
            return next
          })
          setErrors((current) => {
            const next = new Map(current)
            if (result.ok) next.delete(result.padIndex)
            else next.set(result.padIndex, `${result.name}: ${result.error ?? 'load failed'}`)
            return next
          })
          if (result.ok) {
            setSamples((current) => current.map((entry, index) => (index === result.padIndex ? result.sample : entry)))
            setKit((current) => ({
              ...current,
              pads: current.pads.map((pad, index) =>
                index === result.padIndex ? { ...pad, sampleName: result.name } : pad,
              ),
            }))
          }
        },
        onPadLevels: setLevels,
        onOutput: (peak, rms) => setOutput({ peak, rms }),
        onFiles: setFiles,
        onFileDrag: (point) =>
          setDrag((current) => {
            if (!point) return null
            const zone = dropZoneAtPoint(point.x, point.y)
            if (current && current.count === point.count && sameZone(current.zone, zone)) return current
            return { zone, count: point.count }
          }),
        onFileDrop: (drop) => {
          setDrag(null)
          placeDropRef.current(drop)
        },
      }),
    [],
  )

  const setPadField = useCallback(<K extends PadField>(padIndex: number, field: K, value: Pad[K]) => {
    setKit((current) => ({
      ...current,
      pads: current.pads.map((pad, index) => (index === padIndex ? { ...pad, [field]: value } : pad)),
    }))
    postParam(padParamId(padIndex, field), wireValue(field, value))
  }, [])

  const setMaster = useCallback((field: 'masterGain' | 'masterTune', value: number) => {
    setKit((current) => ({ ...current, [field]: value }))
    postParam(field, value)
  }, [])

  const load = (fileName: string) => {
    setLoading((current) => new Set(current).add(selected))
    postLoadSample(selected, fileName)
  }
  // No loading mark here: the picker can be cancelled, and nothing reports
  // that back. The pad updates when the load result arrives.
  const browse = () => postBrowseSample(selected)

  // Native owns OS file drags: it reports the pointer while files hover
  // (`fileDrag`), then keeps the dropped files and asks where they go
  // (`fileDrop`). The page only answers with the pad under the point.
  const placeDrop = (drop: FileDrop) => {
    const zone = dropZoneAtPoint(drop.x, drop.y)
    if (drop.fileNames.length === 0) {
      setDropNote('Only WAV, AIFF, FLAC or MP3 files can be loaded')
      return
    }
    setDropNote(drop.rejected > 0 ? `Skipped ${drop.rejected} file(s) that are not audio` : null)
    if (zone.kind === 'library') {
      postPlaceDroppedFiles(drop.dropId, null)
      return
    }
    // Onto consecutive pads from the drop target; past the last pad a file
    // is only added to the Samples folder.
    const first = zone.kind === 'pad' ? zone.index : selected
    setLoading((current) => {
      const next = new Set(current)
      for (const index of dropPads(first, drop.fileNames.length)) next.add(index)
      return next
    })
    setSelected(first)
    postPlaceDroppedFiles(drop.dropId, first)
  }
  const placeDropRef = useRef(placeDrop)
  placeDropRef.current = placeDrop

  useEffect(() => {
    if (!dropNote) return
    const timer = window.setTimeout(() => setDropNote(null), 3000)
    return () => window.clearTimeout(timer)
  }, [dropNote])

  const dropTargets =
    drag && drag.zone.kind !== 'library'
      ? dropPads(drag.zone.kind === 'pad' ? drag.zone.index : selected, Math.max(1, drag.count))
      : []
  const dropHint = !drag
    ? dropNote
    : drag.zone.kind === 'library'
      ? 'Drop to add to the Samples folder'
      : `Drop to load onto ${padRange(dropTargets)}`

  const pad = kit.pads[selected]!
  const sample = samples[selected] ?? null
  const change = <K extends PadField>(field: K, value: Pad[K]) => setPadField(selected, field, value)
  const loadedCount = kit.pads.filter((entry) => entry.sampleName).length
  const padLabel = `Pad ${String(selected + 1).padStart(2, '0')}`
  const error = errors.get(selected)

  return (
    <main className="app">
      <header className="topbar">
        <div className="brand">
          <svg viewBox="0 0 24 24" className="brand-mark" aria-hidden="true">
            <rect x="1" y="1" width="22" height="22" rx="6" />
            {[0, 1, 2].flatMap((row) =>
              [0, 1, 2].map((col) => (
                <rect
                  key={`${row}${col}`}
                  x={5 + col * 5}
                  y={5 + row * 5}
                  width={4}
                  height={4}
                  rx={1}
                  className={row === 2 && col === 0 ? 'lit' : ''}
                />
              )),
            )}
          </svg>
          <h1>DRUM SAMPLER</h1>
          <span className="brand-sub">16-pad one-shot kit</span>
          <span
            className={`status-dot ${connected ? 'is-live' : ''}`}
            title={connected ? 'Linked to the insert' : 'Preview — no insert bound'}
          />
        </div>
        <span className="kit-count num">
          {loadedCount} / {PAD_COUNT} pads loaded
        </span>
        <div className="master">
          <span className="cap">Master</span>
          <Knob
            label="Gain"
            value={kit.masterGain}
            min={RANGE.masterGain[0]}
            max={RANGE.masterGain[1]}
            step={0.1}
            unit="dB"
            format={formatDb}
            defaultValue={0}
            originAtDefault
            size={30}
            onChange={(v) => setMaster('masterGain', v)}
          />
          <Knob
            label="Tune"
            value={kit.masterTune}
            min={RANGE.masterTune[0]}
            max={RANGE.masterTune[1]}
            step={1}
            unit="st"
            format={formatSemis}
            defaultValue={0}
            originAtDefault
            size={30}
            onChange={(v) => setMaster('masterTune', v)}
          />
          <div className="out-meter" title="Kit output" aria-label="Kit output level">
            <span className="cap">Out</span>
            <div className="out-meter-bar">
              <span className="out-rms" style={{ width: `${levelToUnit(output.rms) * 100}%` }} />
              <span className="out-peak" style={{ left: `calc(${levelToUnit(output.peak) * 100}% - 1px)` }} />
            </div>
          </div>
        </div>
      </header>

      <div className="body">
        <div className="left">
          <PadGrid
            pads={kit.pads}
            samples={samples}
            levels={levels}
            selected={selected}
            loading={loading}
            errors={errors}
            dropTargets={dropTargets}
            onSelect={setSelected}
            onAdjust={(index, field, value) => setPadField(index, field, value)}
          />
          <SampleLibrary
            files={files}
            current={pad.sampleName}
            padLabel={padLabel}
            canLoad={connected}
            onLoad={load}
            onBrowse={browse}
            onRefresh={postListSamples}
            dropActive={drag?.zone.kind === 'library'}
          />
        </div>

        <div className="inspector">
          <div className="inspector-head">
            <span className="inspector-pad num">
              {padLabel} · {noteName(pad.note)}
            </span>
            <strong className="inspector-name">{loading.has(selected) ? 'Loading…' : (pad.sampleName ?? 'No sample')}</strong>
            {error && <span className="inspector-error">{error}</span>}
          </div>

          <WaveformEditor
            pad={pad}
            sample={sample}
            masterTune={kit.masterTune}
            onRegion={(edge, value) => change(edge, value)}
          />

          <div className="modules">
            <Module title="Voice">
              <Knob label="Tune" value={pad.tune} min={RANGE.tune[0]} max={RANGE.tune[1]} step={1} unit="st" format={formatSemis} defaultValue={0} originAtDefault onChange={(v) => change('tune', v)} />
              <Knob label="Gain" value={pad.gain} min={RANGE.gain[0]} max={RANGE.gain[1]} step={0.1} unit="dB" format={formatDb} defaultValue={0} originAtDefault onChange={(v) => change('gain', v)} />
              <Knob label="Pan" value={pad.pan} min={RANGE.pan[0]} max={RANGE.pan[1]} step={0.01} format={formatPan} defaultValue={0} originAtDefault onChange={(v) => change('pan', v)} />
              <Knob label="Velocity" value={pad.velocity} min={RANGE.velocity[0]} max={RANGE.velocity[1]} step={1} unit="%" format={formatPercent} defaultValue={100} onChange={(v) => change('velocity', v)} />
            </Module>

            <Module title="Envelope" aside={<span className="module-note">{pad.decay > 0 ? 'Attack · Hold · Decay' : 'Plays to the end'}</span>}>
              <Knob label="Attack" value={pad.attack} min={RANGE.attack[0]} max={RANGE.attack[1]} step={0.1} format={(v) => formatMs(v)} defaultValue={1} {...attackTravel} onChange={(v) => change('attack', v)} />
              <Knob label="Hold" value={pad.hold} min={RANGE.hold[0]} max={RANGE.hold[1]} step={1} format={(v) => formatMs(v)} defaultValue={0} disabled={pad.decay <= 0} {...holdTravel} onChange={(v) => change('hold', v)} />
              <Knob label="Decay" value={pad.decay} min={RANGE.decay[0]} max={RANGE.decay[1]} step={1} format={(v) => (v <= 0 ? 'Off' : formatMs(v))} defaultValue={0} {...decayTravel} onChange={(v) => change('decay', v)} />
            </Module>

            <Module
              title="Filter"
              aside={
                <Segmented<FilterMode>
                  label="Filter mode"
                  value={pad.filterMode}
                  options={FILTER_MODES.map((mode) => ({ value: mode, label: FILTER_LABELS[mode] }))}
                  onChange={(mode) => change('filterMode', mode)}
                />
              }
            >
              <FilterCurve pad={pad} />
              <Knob label="Cutoff" value={pad.cutoff} min={RANGE.cutoff[0]} max={RANGE.cutoff[1]} step={1} unit="Hz" format={formatHz} defaultValue={20_000} disabled={pad.filterMode === 'off'} {...cutoffTravel} onChange={(v) => change('cutoff', v)} />
              <Knob label="Reso" value={pad.resonance} min={RANGE.resonance[0]} max={RANGE.resonance[1]} step={1} unit="%" format={formatPercent} defaultValue={0} disabled={pad.filterMode === 'off'} onChange={(v) => change('resonance', v)} />
            </Module>

            <Module title="Trigger">
              <Stepper label="Note" value={pad.note} min={0} max={127} display={noteName} onChange={(v) => change('note', v)} />
              <Stepper label="Choke" value={pad.choke} min={RANGE.choke[0]} max={RANGE.choke[1]} display={(v) => (v === 0 ? 'Off' : `Group ${v}`)} onChange={(v) => change('choke', v)} />
              <div className="toggles">
                <Toggle label="Reverse" on={pad.reverse} title="Play the region backwards" onToggle={() => change('reverse', !pad.reverse)} />
                <Toggle label="Mute" on={pad.mute} title="Silence this pad" onToggle={() => change('mute', !pad.mute)} />
                <Toggle label="Solo" on={pad.solo} tone="warn" title="Hear only soloed pads" onToggle={() => change('solo', !pad.solo)} />
              </div>
            </Module>
          </div>
        </div>
      </div>

      {dropHint && (
        <div className={drag ? 'drop-hint' : 'drop-hint is-note'} role="status">
          {dropHint}
        </div>
      )}
    </main>
  )
}

export default App
