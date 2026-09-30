import { useMemo } from 'react'
import { BypassNote, Group, Segmented, Toggle } from './components/Controls'
import { Header } from './components/Header'
import { Knob } from './components/Knob'
import { Meters } from './components/Meters'
import { Stage, type StageMarker } from './components/Stage'
import { formatDb, formatInt, formatMs, logTravel } from './lib/math'
import {
  DEFAULT_PARAMS,
  PLUGIN_ID,
  RANGES,
  STYLE_OPTIONS,
  parseParams,
  wireValues,
} from './lib/params'
import { FACTORY_PRESETS, matchingPresetIndex } from './lib/presets'
import { useEditor } from './lib/useEditor'

const RELEASE_TRAVEL = logTravel(RANGES.releaseMs[0], RANGES.releaseMs[1])

const MARK = (
  <svg viewBox="0 0 24 24" className="h-5 w-5">
    <rect x="1" y="1" width="22" height="22" rx="6" fill="var(--color-raised)" />
    <path d="M4 7h16" stroke="var(--color-warn)" strokeWidth="1.6" strokeLinecap="round" />
    <path
      d="M4 17l3-6 3 4 3-8 3 6 4-4"
      fill="none"
      stroke="var(--color-accent)"
      strokeWidth="1.8"
      strokeLinecap="round"
      strokeLinejoin="round"
    />
  </svg>
)

function App() {
  const editor = useEditor({
    pluginId: PLUGIN_ID,
    defaults: DEFAULT_PARAMS,
    parse: parseParams,
    presets: FACTORY_PRESETS,
    match: matchingPresetIndex,
    wireValues,
  })
  const { params, update } = editor

  const markers = useMemo<StageMarker[]>(
    () => [{ db: params.ceilingDb, label: `Ceiling ${formatDb(params.ceilingDb)} dB`, tone: 'warn' }],
    [params.ceilingDb],
  )

  return (
    <main className="flex h-full w-full flex-col overflow-hidden bg-window">
      <Header
        name="BurnLimit"
        subtitle="Loudness maximizer"
        mark={MARK}
        connected={editor.connected}
        presets={FACTORY_PRESETS}
        presetIndex={editor.presetIndex}
        onPreset={editor.loadPreset}
        power={params.power}
        onPower={(power) => update({ power })}
      />

      <div className="flex min-h-0 flex-1 gap-2 p-3 pb-2">
        <section
          className={`relative min-h-[160px] min-w-0 flex-1 overflow-hidden rounded-lg border border-line bg-floor transition-opacity duration-200 ${
            params.power ? '' : 'opacity-70'
          }`}
        >
          <Stage historyRef={editor.historyRef} markers={markers} reductionLabel="GR" active={params.power} />
          {!params.power && <BypassNote name="BurnLimit" />}
        </section>
        <aside className="flex w-[184px] shrink-0 flex-col rounded-lg border border-line bg-panel px-2 pt-3 pb-2">
          <Meters historyRef={editor.historyRef} reductionLabel="GR" active={params.power} />
        </aside>
      </div>

      <section className="mx-3 mb-3 flex shrink-0 flex-wrap items-start gap-x-8 gap-y-3 rounded-lg border border-line bg-panel px-4 pt-3 pb-2">
        <Group title="Style">
          <div className="flex flex-col gap-2">
            <Segmented label="Style" value={params.style} options={STYLE_OPTIONS} onChange={(style) => update({ style })} />
            <span className="text-[11px] text-ink-3">
              {STYLE_OPTIONS.find((option) => option.value === params.style)?.title}
            </span>
          </div>
        </Group>
        <Group title="Loudness">
          <Knob
            label="Gain"
            value={params.gainDb}
            min={RANGES.gainDb[0]}
            max={RANGES.gainDb[1]}
            step={0.1}
            unit="dB"
            format={formatDb}
            defaultValue={DEFAULT_PARAMS.gainDb}
            originAtDefault
            size={52}
            onChange={(gainDb) => update({ gainDb })}
          />
          <Knob
            label="Ceiling"
            value={params.ceilingDb}
            min={RANGES.ceilingDb[0]}
            max={RANGES.ceilingDb[1]}
            step={0.1}
            unit="dB"
            format={formatDb}
            defaultValue={DEFAULT_PARAMS.ceilingDb}
            accent="var(--color-warn)"
            size={52}
            onChange={(ceilingDb) => update({ ceilingDb })}
          />
        </Group>
        <Group title="Timing">
          <Knob
            label="Release"
            value={params.releaseMs}
            min={RANGES.releaseMs[0]}
            max={RANGES.releaseMs[1]}
            step={1}
            unit="ms"
            format={formatMs}
            defaultValue={DEFAULT_PARAMS.releaseMs}
            {...RELEASE_TRAVEL}
            size={44}
            onChange={(releaseMs) => update({ releaseMs })}
          />
          <Knob
            label="Lookahead"
            value={params.lookaheadMs}
            min={RANGES.lookaheadMs[0]}
            max={RANGES.lookaheadMs[1]}
            step={0.1}
            unit="ms"
            format={(ms) => ms.toFixed(1)}
            defaultValue={DEFAULT_PARAMS.lookaheadMs}
            size={44}
            onChange={(lookaheadMs) => update({ lookaheadMs })}
          />
        </Group>
        <Group title="Output" className="ml-auto">
          <Knob
            label="Mix"
            value={params.mix}
            min={RANGES.mix[0]}
            max={RANGES.mix[1]}
            step={1}
            unit="%"
            format={formatInt}
            defaultValue={DEFAULT_PARAMS.mix}
            size={44}
            onChange={(mix) => update({ mix })}
          />
          <div className="flex flex-col gap-2 pt-4">
            <Toggle
              label="True Peak"
              on={params.truePeak}
              title="Hold inter-sample peaks under the ceiling too"
              onToggle={() => update({ truePeak: !params.truePeak })}
            />
            <Toggle
              label="Stereo Link"
              on={params.stereoLink}
              title="Reduce both channels together so the image does not shift"
              onToggle={() => update({ stereoLink: !params.stereoLink })}
            />
          </div>
        </Group>
      </section>
    </main>
  )
}

export default App
