import { useMemo } from 'react'
import { BypassNote, Group, Segmented, Toggle } from './components/Controls'
import { Header } from './components/Header'
import { Knob } from './components/Knob'
import { Meters } from './components/Meters'
import { Stage, type StageMarker } from './components/Stage'
import { formatDb, formatInt } from './lib/math'
import { DEFAULT_PARAMS, MODE_OPTIONS, PLUGIN_ID, RANGES, parseParams, wireValues } from './lib/params'
import { FACTORY_PRESETS, matchingPresetIndex } from './lib/presets'
import { useEditor } from './lib/useEditor'

const MARK = (
  <svg viewBox="0 0 24 24" className="h-5 w-5">
    <rect x="1" y="1" width="22" height="22" rx="6" fill="var(--color-raised)" />
    <path d="M4 8h16M4 16h16" stroke="var(--color-ink-4)" strokeWidth="1.2" strokeDasharray="2 2" />
    <path
      d="M4 12c2-6 3-4 4-4h8c1 0 2-2 4 4"
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
    () => [
      { db: params.thresholdDb, label: `Threshold ${formatDb(params.thresholdDb)} dB`, tone: 'accent' },
      { db: params.ceilingDb, label: `Ceiling ${formatDb(params.ceilingDb)} dB`, tone: 'warn' },
    ],
    [params.thresholdDb, params.ceilingDb],
  )

  return (
    <main className="flex h-full w-full flex-col overflow-hidden bg-window">
      <Header
        name="67Clipper"
        subtitle="Clipper and peak limiter"
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
          <Stage historyRef={editor.historyRef} markers={markers} reductionLabel="Clip" active={params.power} />
          {!params.power && <BypassNote name="67Clipper" />}
        </section>
        <aside className="flex w-[184px] shrink-0 flex-col rounded-lg border border-line bg-panel px-2 pt-3 pb-2">
          <Meters historyRef={editor.historyRef} reductionLabel="Clip" active={params.power} />
        </aside>
      </div>

      <section className="mx-3 mb-3 flex shrink-0 flex-wrap items-start gap-x-8 gap-y-3 rounded-lg border border-line bg-panel px-4 pt-3 pb-2">
        <Group title="Mode">
          <div className="flex flex-col gap-2">
            <Segmented label="Mode" value={params.mode} options={MODE_OPTIONS} onChange={(mode) => update({ mode })} />
            <span className="text-[11px] text-ink-3">
              {MODE_OPTIONS.find((option) => option.value === params.mode)?.title}
            </span>
          </div>
        </Group>
        <Group title="Clipping">
          <Knob
            label="Threshold"
            value={params.thresholdDb}
            min={RANGES.thresholdDb[0]}
            max={RANGES.thresholdDb[1]}
            step={0.1}
            unit="dB"
            format={formatDb}
            defaultValue={DEFAULT_PARAMS.thresholdDb}
            size={52}
            onChange={(thresholdDb) => update({ thresholdDb })}
          />
          <Knob
            label="Shape"
            value={params.shape}
            min={RANGES.shape[0]}
            max={RANGES.shape[1]}
            step={1}
            unit="%"
            format={formatInt}
            defaultValue={DEFAULT_PARAMS.shape}
            size={52}
            onChange={(shape) => update({ shape })}
          />
          <div className="flex flex-col justify-center gap-1 pt-5 text-[10.5px] text-ink-4">
            <span>0 % hard</span>
            <span>100 % soft</span>
          </div>
        </Group>
        <Group title="Output">
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
            size={44}
            onChange={(ceilingDb) => update({ ceilingDb })}
          />
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
        </Group>
        <Group title="Options" className="ml-auto">
          <div className="flex flex-col gap-2">
            <Toggle
              label="DC Filter"
              on={params.dcFilter}
              title="Block DC (a 5 Hz high-pass on the output)"
              onToggle={() => update({ dcFilter: !params.dcFilter })}
            />
            <Toggle
              label="Stereo Link"
              on={params.stereoLink}
              title="Link the peak detector across both channels"
              onToggle={() => update({ stereoLink: !params.stereoLink })}
            />
          </div>
        </Group>
      </section>
    </main>
  )
}

export default App
