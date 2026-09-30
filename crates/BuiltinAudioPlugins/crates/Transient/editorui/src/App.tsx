import { useMemo } from 'react'
import { BypassNote, Group, Toggle } from './components/Controls'
import { Header } from './components/Header'
import { Knob } from './components/Knob'
import { Meters } from './components/Meters'
import { Stage, type StageMarker } from './components/Stage'
import { formatInt, formatSigned } from './lib/math'
import {
  DEFAULT_PARAMS,
  MAX_SHAPE_DB,
  PLUGIN_ID,
  RANGES,
  describeShape,
  parseParams,
  wireValues,
} from './lib/params'
import { FACTORY_PRESETS, matchingPresetIndex } from './lib/presets'
import { useEditor } from './lib/useEditor'

const MARK = (
  <svg viewBox="0 0 24 24" className="h-5 w-5">
    <rect x="1" y="1" width="22" height="22" rx="6" fill="var(--color-raised)" />
    <path
      d="M4 18V7l3 5c3 2 7 3 13 4"
      fill="none"
      stroke="var(--color-accent)"
      strokeWidth="1.8"
      strokeLinecap="round"
      strokeLinejoin="round"
    />
  </svg>
)

const NO_MARKERS: StageMarker[] = []

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

  return (
    <main className="flex h-full w-full flex-col overflow-hidden bg-window">
      <Header
        name="Transient"
        subtitle="Attack and sustain shaper"
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
          <Stage historyRef={editor.historyRef} markers={NO_MARKERS} reductionLabel="Shape" active={params.power} />
          {!params.power && <BypassNote name="Transient" />}
        </section>
        <aside className="flex w-[184px] shrink-0 flex-col rounded-lg border border-line bg-panel px-2 pt-3 pb-2">
          <Meters historyRef={editor.historyRef} reductionLabel="Shape" reductionSign="" active={params.power} />
        </aside>
      </div>

      <section className="mx-3 mb-3 flex shrink-0 flex-wrap items-start gap-x-8 gap-y-3 rounded-lg border border-line bg-panel px-4 pt-3 pb-2">
        <Group title="Shape">
          <Knob
            label="Attack"
            value={params.attack}
            min={RANGES.attack[0]}
            max={RANGES.attack[1]}
            step={1}
            unit="%"
            format={formatSigned}
            defaultValue={DEFAULT_PARAMS.attack}
            originAtDefault
            size={56}
            onChange={(attack) => update({ attack })}
          />
          <Knob
            label="Sustain"
            value={params.sustain}
            min={RANGES.sustain[0]}
            max={RANGES.sustain[1]}
            step={1}
            unit="%"
            format={formatSigned}
            defaultValue={DEFAULT_PARAMS.sustain}
            originAtDefault
            size={56}
            onChange={(sustain) => update({ sustain })}
          />
          <EnvelopeSketch attack={params.attack} sustain={params.sustain} />
        </Group>
        <Group title="Detector">
          <Knob
            label="Speed"
            value={params.speed}
            min={RANGES.speed[0]}
            max={RANGES.speed[1]}
            step={1}
            unit="%"
            format={formatInt}
            defaultValue={DEFAULT_PARAMS.speed}
            size={44}
            onChange={(speed) => update({ speed })}
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
          <div className="pt-4">
            <Toggle
              label="Stereo Link"
              on={params.stereoLink}
              title="Shape both channels from one detector so the image does not shift"
              onToggle={() => update({ stereoLink: !params.stereoLink })}
            />
          </div>
        </Group>
      </section>
    </main>
  )
}

/**
 * What the two amounts do to one hit: the dry envelope dashed, the shaped one
 * solid, with the attack region scaled by Attack and the body by Sustain at
 * the plugin's ±18 dB depth. A drawing of the settings, not a measurement.
 */
function EnvelopeSketch({ attack, sustain }: { attack: number; sustain: number }) {
  const { dry, shaped } = useMemo(() => {
    const w = 150
    const h = 58
    const top = 6
    const scale = (h - top - 4) / 1.6
    const points = 60
    const attackGain = Math.pow(10, ((attack / 100) * MAX_SHAPE_DB) / 20)
    const sustainGain = Math.pow(10, ((sustain / 100) * MAX_SHAPE_DB) / 20)
    let dryPath = ''
    let shapedPath = ''
    for (let i = 0; i <= points; i++) {
      const t = i / points
      const envelope = t < 0.03 ? t / 0.03 : Math.exp(-(t - 0.03) / 0.35)
      // The first fifth after the hit is the attack region, the rest the body.
      const blend = Math.min(1, Math.max(0, (t - 0.08) / 0.12))
      const gain = attackGain * (1 - blend) + sustainGain * blend
      const x = 4 + t * (w - 8)
      const yDry = h - 4 - envelope * scale
      const yShaped = h - 4 - Math.min(envelope * gain, 1.6) * scale
      dryPath += `${i === 0 ? 'M' : 'L'}${x.toFixed(1)} ${yDry.toFixed(1)}`
      shapedPath += `${i === 0 ? 'M' : 'L'}${x.toFixed(1)} ${yShaped.toFixed(1)}`
    }
    return { dry: dryPath, shaped: shapedPath }
  }, [attack, sustain])

  return (
    <div className="flex flex-col gap-1 pt-1">
      <svg
        viewBox="0 0 150 58"
        className="h-[58px] w-[150px] rounded-md border border-line bg-floor"
        role="img"
        aria-label={`Attack ${describeShape(attack)}, sustain ${describeShape(sustain)}`}
      >
        <path d={dry} fill="none" stroke="var(--color-ink-4)" strokeWidth="1" strokeDasharray="3 3" />
        <path d={shaped} fill="none" stroke="var(--color-accent)" strokeWidth="1.6" strokeLinejoin="round" />
      </svg>
      <span className="num text-[10.5px] text-ink-3">
        Attack {describeShape(attack)} · Sustain {describeShape(sustain)}
      </span>
    </div>
  )
}

export default App
