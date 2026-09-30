import { useEffect, useRef, useState } from 'react'
import {
  connectBridge,
  defaults,
  postGlobalCommand,
  postParam,
  type BoundInstance,
  type MeterFrame,
  type MixStationParams,
  type SpectrumFrame,
} from './bridge'
import { ChainList } from './ChainList'
import { BypassNote } from './Controls'
import { Header } from './Header'
import { MeterHistory } from './history'
import { InstanceRoute } from './InstanceRoute'
import { Meters } from './Meters'
import { ModuleEditor } from './ModuleEditor'
import { RACK_MODULES, SLOT_IDS, moduleByCode, slotCodes, type BooleanParamId, type RackModule } from './modules'
import { ParamKnob } from './ParamKnob'
import { PARAM_SPECS, sanitizeParams, type NumericParamId } from './params'
import { FACTORY_PRESETS, matchingPresetIndex, postAllParams } from './presets'

const MARK = (
  <svg viewBox="0 0 24 24" className="h-5 w-5">
    <rect x="1" y="1" width="22" height="22" rx="6" fill="var(--color-raised)" />
    <path d="M6 7h12M6 12h12M6 17h12" stroke="var(--color-ink-4)" strokeWidth="1.6" strokeLinecap="round" />
    <path d="M9 5v4M15 10v4M11 15v4" stroke="var(--color-accent)" strokeWidth="1.8" strokeLinecap="round" />
  </svg>
)

export default function App() {
  const [params, setParams] = useState<MixStationParams>(defaults)
  const [connected, setConnected] = useState(false)
  /** Which plug-in instance the shell has bound this page to, if any. */
  const [instance, setInstance] = useState<BoundInstance | null>(null)
  const [spectrumLive, setSpectrumLive] = useState(false)
  const [presetIndex, setPresetIndex] = useState<number | null>(() => matchingPresetIndex(defaults))
  const [selected, setSelected] = useState<number | null>(null)
  const historyRef = useRef(new MeterHistory())
  const stageRef = useRef<MeterFrame | null>(null)
  const spectrumRef = useRef<SpectrumFrame | null>(null)

  // DAW transport owns bare Space on the editor surface. Forward it when the
  // native claim path misses an off-screen/focus edge case.
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.ctrlKey || event.metaKey || event.altKey || event.repeat) return
      if (event.key !== ' ' && event.code !== 'Space') return
      const target = event.target as HTMLElement | null
      if (target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.isContentEditable)) return
      event.preventDefault()
      postGlobalCommand('transport:play-pause')
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [])

  useEffect(() => {
    const dropTelemetry = () => {
      historyRef.current.clear()
      stageRef.current = null
      spectrumRef.current = null
      setSpectrumLive(false)
    }
    return connectBridge(
      (incoming) => {
        const clean = sanitizeParams(incoming)
        setParams(clean)
        setPresetIndex(matchingPresetIndex(clean))
        // View state and telemetry belong to the previously bound instance:
        // the shell reuses one browser per plug-in id and rebinds it.
        setSelected(null)
        dropTelemetry()
      },
      (isConnected, bound) => {
        setConnected(isConnected)
        setInstance(bound)
        if (!isConnected) dropTelemetry()
      },
      (frame) => {
        stageRef.current = frame
        historyRef.current.push(frame)
      },
      (frame) => {
        spectrumRef.current = frame
        setSpectrumLive(true)
      },
    )
  }, [])

  const apply = (next: MixStationParams) => {
    const clean = sanitizeParams(next)
    setParams(clean)
    setPresetIndex(matchingPresetIndex(clean))
    return clean
  }

  const changeNumber = (id: NumericParamId, value: number) => {
    apply({ ...params, [id]: value })
    postParam(id, value)
  }

  const changeBoolean = (id: BooleanParamId, value: boolean) => {
    apply({ ...params, [id]: value })
    postParam(id, value ? 1 : 0)
  }

  const codes = slotCodes(params)
  const loaded = codes.filter((code) => code !== 0)
  const modules = loaded.map((code) => moduleByCode(code)).filter((module): module is RackModule => !!module)
  const available = RACK_MODULES.filter((module) => !codes.includes(module.code))
  const current = modules.find((module) => module.code === selected) ?? modules[0] ?? null

  /** Write an ordered list of module codes back into the six slots, as one batch. */
  const writeOrder = (order: readonly number[], extra: Partial<MixStationParams> = {}) => {
    const next: MixStationParams = { ...params, ...extra }
    SLOT_IDS.forEach((id, index) => {
      next[id] = order[index] ?? 0
    })
    const clean = apply(next)
    for (const id of SLOT_IDS) postParam(id, clean[id])
    for (const module of RACK_MODULES) postParam(module.enabledId, clean[module.enabledId] ? 1 : 0)
  }

  const addModule = (module: RackModule) => {
    if (codes.includes(module.code)) return
    writeOrder([...loaded, module.code], { [module.enabledId]: true })
    setSelected(module.code)
  }

  const removeModule = (module: RackModule) => {
    writeOrder(
      loaded.filter((code) => code !== module.code),
      { [module.enabledId]: false },
    )
  }

  /** Restore one module's parameters to the Rust-authored defaults. */
  const resetModule = (module: RackModule) => {
    const next = { ...params }
    for (const id of [...module.knobs, module.trimId]) next[id] = defaults[id]
    apply(next)
    for (const id of [...module.knobs, module.trimId]) postParam(id, defaults[id])
  }

  const loadPreset = (index: number) => {
    const entry = FACTORY_PRESETS[index]
    if (!entry) return
    const next = sanitizeParams({ ...entry.params })
    setParams(next)
    setPresetIndex(index)
    postAllParams(next)
  }

  const display = instance?.display
  return (
    <main className="flex h-full w-full flex-col overflow-hidden bg-window">
      {/* Mirrors the approved binding into the URL; never binds on its own. */}
      <InstanceRoute instance={instance} />
      <Header
        name="MixStation"
        subtitle="Channel strip rack"
        mark={MARK}
        connected={connected}
        presets={FACTORY_PRESETS}
        presetIndex={presetIndex}
        onPreset={loadPreset}
        power={params.power}
        onPower={(power) => changeBoolean('power', power)}
        detail={
          display && (
            <span className="hidden min-w-0 truncate text-[11px] text-ink-3 xl:block" title="The insert this editor is bound to">
              {display.trackName} · {display.insertName}
            </span>
          )
        }
      />

      <div className="flex min-h-0 flex-1 gap-2 p-3">
        <aside className="flex w-[244px] shrink-0 flex-col rounded-lg border border-line bg-panel p-2.5">
          <ChainList
            modules={modules}
            available={available}
            selected={current?.code ?? null}
            enabled={(module) => params[module.enabledId]}
            powered={params.power}
            stageRef={stageRef}
            onSelect={setSelected}
            onReorder={(order) => writeOrder(order)}
            onToggle={(module) => changeBoolean(module.enabledId, !params[module.enabledId])}
            onAdd={addModule}
          />
        </aside>

        <div className="relative flex min-w-0 flex-1 flex-col">
          {current ? (
            <ModuleEditor
              module={current}
              position={loaded.indexOf(current.code)}
              params={params}
              on={params[current.enabledId]}
              powered={params.power}
              spectrumRef={spectrumRef}
              spectrumLive={connected && spectrumLive}
              onNumber={changeNumber}
              onToggle={() => changeBoolean(current.enabledId, !params[current.enabledId])}
              onReset={() => resetModule(current)}
              onRemove={() => removeModule(current)}
            />
          ) : (
            <div className="grid flex-1 place-items-center rounded-lg border border-dashed border-line-hi text-center">
              <div className="flex max-w-72 flex-col gap-1">
                <span className="text-[13px] font-semibold">No modules in the chain</span>
                <span className="text-[11.5px] text-ink-3">
                  Add Filters, EQ, Compressor, Drive, Width or Limiter from the signal path on the left, in any order.
                </span>
              </div>
            </div>
          )}
          {!params.power && <BypassNote name="MixStation" />}
        </div>

        <aside className="flex w-[176px] shrink-0 flex-col items-center gap-2 rounded-lg border border-line bg-panel px-2 pt-3 pb-2">
          <ParamKnob
            spec={PARAM_SPECS.inputTrimDb}
            value={params.inputTrimDb}
            bipolar
            size={40}
            disabled={!params.power}
            disabledHint="MixStation is bypassed"
            onChange={(value) => changeNumber('inputTrimDb', value)}
          />
          <div className="min-h-0 w-full flex-1">
            <Meters historyRef={historyRef} reductionLabel="GR" active={params.power} />
          </div>
          <ParamKnob
            spec={PARAM_SPECS.outputTrimDb}
            value={params.outputTrimDb}
            bipolar
            size={40}
            disabled={!params.power}
            disabledHint="MixStation is bypassed"
            onChange={(value) => changeNumber('outputTrimDb', value)}
          />
        </aside>
      </div>
    </main>
  )
}
