import type { ReactNode, RefObject } from 'react'
import { ArrowCounterClockwiseIcon, TrashIcon } from '@phosphor-icons/react'
import type { MixStationParams, SpectrumFrame } from './bridge'
import { IconButton, Toggle } from './Controls'
import {
  CompressorDisplay,
  EqDisplay,
  FilterDisplay,
  LimiterDisplay,
  SaturationDisplay,
  WidthDisplay,
  type CurveChange,
} from './Curves'
import type { RackModule } from './modules'
import { ParamKnob } from './ParamKnob'
import { PARAM_SPECS, type NumericParamId } from './params'

/// Gains and trims fill out from 0 dB rather than from the bottom of the range.
const BIPOLAR: ReadonlySet<NumericParamId> = new Set([
  'lowGainDb',
  'lowMidGainDb',
  'highMidGainDb',
  'highGainDb',
  'compMakeupDb',
])

export type ModuleEditorProps = {
  module: RackModule
  position: number
  params: MixStationParams
  on: boolean
  powered: boolean
  spectrumRef: RefObject<SpectrumFrame | null>
  spectrumLive: boolean
  onNumber: CurveChange
  onToggle: () => void
  onReset: () => void
  onRemove: () => void
}

/**
 * The selected rack module: its response drawn large, its controls beneath,
 * and its own output trim set apart at the end of the row.
 */
export function ModuleEditor({
  module,
  position,
  params,
  on,
  powered,
  spectrumRef,
  spectrumLive,
  onNumber,
  onToggle,
  onReset,
  onRemove,
}: ModuleEditorProps) {
  const active = on && powered
  const knob = (id: NumericParamId, size = 46) => (
    <ParamKnob
      key={id}
      spec={PARAM_SPECS[id]}
      value={params[id]}
      bipolar={BIPOLAR.has(id)}
      size={size}
      disabled={!active}
      disabledHint={powered ? `${module.name} is bypassed` : 'MixStation is bypassed'}
      onChange={(value) => onNumber(id, value)}
    />
  )

  return (
    <section className="flex min-h-0 min-w-0 flex-1 flex-col gap-2" aria-label={`${module.name} module`}>
      <div className="flex h-8 shrink-0 items-center gap-3">
        <div className="flex min-w-0 items-baseline gap-2">
          <span className="num text-[11px] text-ink-4">{position + 1}</span>
          <h2 className="text-[13px] font-bold">{module.name}</h2>
          <span className="truncate text-[11px] text-ink-3">{module.hint}</span>
        </div>
        <div className="ml-auto flex items-center gap-1">
          <Toggle
            label={on ? 'On' : 'Bypassed'}
            on={on}
            title={on ? `Bypass ${module.name}` : `Turn ${module.name} on`}
            onToggle={onToggle}
          />
          <IconButton label={`Reset ${module.name} to its defaults`} onClick={onReset}>
            <ArrowCounterClockwiseIcon size={14} weight="bold" />
          </IconButton>
          <IconButton label={`Remove ${module.name} from the chain`} onClick={onRemove}>
            <TrashIcon size={14} weight="bold" />
          </IconButton>
        </div>
      </div>

      <div
        className={`relative min-h-[140px] flex-1 overflow-hidden rounded-lg border border-line bg-floor transition-opacity duration-200 ${
          active ? '' : 'opacity-60'
        }`}
      >
        <Display
          module={module}
          params={params}
          active={active}
          spectrumRef={spectrumRef}
          spectrumLive={spectrumLive}
          onNumber={onNumber}
        />
      </div>

      <div className="flex shrink-0 items-start gap-3 rounded-lg border border-line bg-panel px-3 pt-2.5 pb-1.5">
        <div className="flex min-w-0 flex-1 flex-wrap items-start gap-x-3 gap-y-1">
          {module.knobs.map((id) => knob(id, module.knobs.length > 4 ? 42 : 48))}
        </div>
        <div className="flex shrink-0 items-start gap-2 border-l border-line pl-3">
          <ParamKnob
            spec={PARAM_SPECS[module.trimId]}
            value={params[module.trimId]}
            bipolar
            size={40}
            disabled={!active}
            disabledHint={powered ? `${module.name} is bypassed` : 'MixStation is bypassed'}
            onChange={(value) => onNumber(module.trimId, value)}
          />
        </div>
      </div>
    </section>
  )
}

function Display({
  module,
  params,
  active,
  spectrumRef,
  spectrumLive,
  onNumber,
}: {
  module: RackModule
  params: MixStationParams
  active: boolean
  spectrumRef: RefObject<SpectrumFrame | null>
  spectrumLive: boolean
  onNumber: CurveChange
}): ReactNode {
  switch (module.enabledId) {
    case 'filtersEnabled':
      return (
        <FilterDisplay
          params={params}
          active={active}
          spectrumRef={spectrumRef}
          spectrumLive={spectrumLive}
          onChange={onNumber}
        />
      )
    case 'eqEnabled':
      return (
        <EqDisplay params={params} active={active} spectrumRef={spectrumRef} spectrumLive={spectrumLive} onChange={onNumber} />
      )
    case 'compEnabled':
      return <CompressorDisplay params={params} active={active} onChange={onNumber} />
    case 'satEnabled':
      return <SaturationDisplay params={params} active={active} />
    case 'widthEnabled':
      return <WidthDisplay params={params} active={active} />
    case 'limiterEnabled':
      return <LimiterDisplay params={params} active={active} onChange={onNumber} />
  }
}
