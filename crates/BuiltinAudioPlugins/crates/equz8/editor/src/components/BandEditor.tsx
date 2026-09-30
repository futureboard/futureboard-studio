import type { ReactNode } from 'react'
import { HeadphonesIcon, LightningIcon, PowerIcon } from '@phosphor-icons/react'
import type { Band, EqParams } from '../bridge'
import {
  BAND_COLORS,
  FILTER_KINDS,
  GAIN_RANGE,
  MAX_FREQ,
  MAX_Q,
  MIN_FREQ,
  MIN_Q,
  OUTPUT_MAX_DB,
  OUTPUT_MIN_DB,
  bandHasGain,
  filterKind,
  formatFrequency,
  formatGain,
  formatQ,
  freqToProgress,
  progressToFreq,
  progressToQ,
  qToProgress,
} from '../lib/eq'
import { Knob } from './Knob'
import { Pill } from './Controls'

export type BandEditorProps = {
  band: Band
  defaultBand: Band
  selected: number
  outputDb: number
  mix: number
  soloed: boolean
  onBandChange: (patch: Partial<Band>) => void
  onGlobalChange: (patch: Partial<Pick<EqParams, 'outputDb' | 'mix'>>) => void
  onToggleSolo: () => void
}

function Section({
  title,
  aside,
  children,
  className = '',
}: {
  title: string
  aside?: ReactNode
  children: ReactNode
  className?: string
}) {
  return (
    <section className={`flex min-w-0 flex-col gap-2 ${className}`}>
      <header className="flex h-6 items-center gap-2">
        <span className="cap">{title}</span>
        <span className="flex-1" />
        {aside}
      </header>
      {children}
    </section>
  )
}

const divider = <div aria-hidden className="w-px self-stretch bg-line" />

/// The selected band, all of it: shape, frequency, gain and Q, its dynamic
/// stage, and the EQ's output beside it.
export function BandEditor({
  band,
  defaultBand,
  selected,
  outputDb,
  mix,
  soloed,
  onBandChange,
  onGlobalChange,
  onToggleSolo,
}: BandEditorProps) {
  const kind = filterKind(band.bandType)
  const canGain = bandHasGain(band.bandType)
  const accent = BAND_COLORS[selected] ?? 'var(--color-accent)'
  const dynamicLive = band.dynamic && canGain

  return (
    // Below 880 px the sections tighten, and a window squeezed further still
    // scrolls sideways rather than cutting the Output section off.
    <div className="flex min-h-0 items-stretch gap-4 overflow-x-auto rounded-lg border border-line bg-panel px-4 py-3 max-[880px]:gap-3 max-[880px]:px-3">
      <Section
        title={`Band ${selected + 1}`}
        className="w-[216px] shrink-0 max-[880px]:w-[184px]"
        aside={
          <>
            <Pill
              label={band.active ? 'On' : 'Off'}
              on={band.active}
              accent={accent}
              title={band.active ? 'Switch this band off' : 'Switch this band on'}
              onToggle={() => onBandChange({ active: !band.active })}
            >
              <PowerIcon size={11} weight="bold" />
            </Pill>
            <Pill
              label="Solo"
              on={soloed}
              accent="var(--color-warn)"
              title={soloed ? 'Stop listening (Esc)' : 'Listen to this band alone'}
              onToggle={onToggleSolo}
            >
              <HeadphonesIcon size={11} weight="bold" />
            </Pill>
          </>
        }
      >
        <div className="grid grid-cols-6 gap-1" role="radiogroup" aria-label="Band shape">
          {FILTER_KINDS.map((item) => {
            const active = band.bandType === item.type
            return (
              <button
                key={item.type}
                type="button"
                role="radio"
                aria-checked={active}
                title={item.label}
                aria-label={item.label}
                className="grid h-8 min-w-0 cursor-pointer place-items-center rounded-md border transition-colors duration-150 hover:bg-white/5"
                style={{
                  borderColor: active ? `color-mix(in srgb, ${accent} 55%, transparent)` : 'var(--color-line)',
                  background: active ? `color-mix(in srgb, ${accent} 16%, transparent)` : 'var(--color-canvas)',
                }}
                onClick={() =>
                  onBandChange({
                    bandType: item.type,
                    // A shape with no gain stage has nothing to make dynamic.
                    ...(bandHasGain(item.type) ? {} : { dynamic: false }),
                  })
                }
              >
                <svg viewBox="0 0 32 20" className="h-3.5 w-[80%]" aria-hidden="true">
                  <path
                    d={item.glyph}
                    fill="none"
                    stroke={active ? accent : 'var(--color-ink-3)'}
                    strokeWidth="1.8"
                    strokeLinecap="round"
                  />
                </svg>
              </button>
            )
          })}
        </div>
        <p className="text-[11px] text-ink-3">
          <span className="font-semibold" style={{ color: accent }}>
            {kind.label}
          </span>{' '}
          at <span className="num text-ink-2">{formatFrequency(band.freq)} Hz</span>
        </p>
      </Section>

      {divider}

      <Section title="Filter" className="shrink-0">
        <div className="flex items-start gap-3">
          <Knob
            label="Freq"
            value={band.freq}
            min={MIN_FREQ}
            max={MAX_FREQ}
            step={1}
            unit="Hz"
            format={formatFrequency}
            defaultValue={defaultBand.freq}
            toProgress={freqToProgress}
            fromProgress={progressToFreq}
            size={52}
            accent={accent}
            onChange={(freq) => onBandChange({ freq })}
          />
          <Knob
            label="Gain"
            value={band.gainDb}
            min={-GAIN_RANGE}
            max={GAIN_RANGE}
            step={0.1}
            unit="dB"
            format={formatGain}
            defaultValue={0}
            originAtDefault
            disabled={!canGain}
            disabledHint={`${kind.label} has no gain`}
            size={52}
            accent={accent}
            onChange={(gainDb) => onBandChange({ gainDb })}
          />
          <Knob
            label="Q"
            value={band.q}
            min={MIN_Q}
            max={MAX_Q}
            step={0.01}
            format={formatQ}
            defaultValue={defaultBand.q}
            toProgress={qToProgress}
            fromProgress={progressToQ}
            size={52}
            accent={accent}
            onChange={(q) => onBandChange({ q })}
          />
        </div>
      </Section>

      {divider}

      <Section
        title="Dynamic"
        className="min-w-0 flex-1"
        aside={
          <Pill
            label={band.dynamic ? 'On' : 'Off'}
            on={dynamicLive}
            accent={accent}
            disabled={!canGain}
            title={
              canGain
                ? band.dynamic
                  ? 'Make the gain static again'
                  : 'Let the gain follow the signal: it moves by Range once the band crosses Threshold'
                : `${kind.label} has no gain to make dynamic`
            }
            onToggle={() => canGain && onBandChange({ dynamic: !band.dynamic })}
          >
            <LightningIcon size={11} weight="bold" />
          </Pill>
        }
      >
        <div
          className={`flex items-start gap-3 transition-opacity duration-150 ${
            dynamicLive ? '' : 'opacity-40'
          }`}
        >
          <Knob
            label="Thresh"
            value={band.thresholdDb}
            min={-60}
            max={0}
            step={0.5}
            unit="dB"
            format={formatGain}
            defaultValue={defaultBand.thresholdDb}
            disabled={!dynamicLive}
            disabledHint="Switch Dynamic on first"
            size={40}
            accent={accent}
            onChange={(thresholdDb) => onBandChange({ thresholdDb })}
          />
          <Knob
            label="Range"
            value={band.rangeDb}
            min={-24}
            max={24}
            step={0.1}
            unit="dB"
            format={formatGain}
            defaultValue={0}
            originAtDefault
            disabled={!dynamicLive}
            disabledHint="Switch Dynamic on first"
            size={40}
            accent={accent}
            onChange={(rangeDb) => onBandChange({ rangeDb })}
          />
          <Knob
            label="Attack"
            value={band.attackMs}
            min={0.1}
            max={500}
            step={0.1}
            unit="ms"
            format={(value) => (value < 10 ? value.toFixed(1) : String(Math.round(value)))}
            defaultValue={defaultBand.attackMs}
            disabled={!dynamicLive}
            disabledHint="Switch Dynamic on first"
            size={40}
            accent={accent}
            onChange={(attackMs) => onBandChange({ attackMs })}
          />
          <Knob
            label="Release"
            value={band.releaseMs}
            min={1}
            max={5000}
            step={1}
            unit="ms"
            format={(value) => String(Math.round(value))}
            defaultValue={defaultBand.releaseMs}
            disabled={!dynamicLive}
            disabledHint="Switch Dynamic on first"
            size={40}
            accent={accent}
            onChange={(releaseMs) => onBandChange({ releaseMs })}
          />
        </div>
      </Section>

      {divider}

      <Section title="Output" className="shrink-0">
        <div className="flex items-start gap-3">
          <Knob
            label="Level"
            value={outputDb}
            min={OUTPUT_MIN_DB}
            max={OUTPUT_MAX_DB}
            step={0.1}
            unit="dB"
            format={formatGain}
            defaultValue={0}
            originAtDefault
            size={44}
            onChange={(value) => onGlobalChange({ outputDb: value })}
          />
          <Knob
            label="Mix"
            value={mix}
            min={0}
            max={100}
            step={1}
            unit="%"
            format={(value) => `${Math.round(value)}`}
            defaultValue={100}
            size={44}
            onChange={(value) => onGlobalChange({ mix: value })}
          />
        </div>
      </Section>
    </div>
  )
}
