import { useCallback, useEffect, useRef, useState } from 'react'
import { AnimatePresence } from 'motion/react'
import {
  ArrowCounterClockwiseIcon,
  CaretDownIcon,
  CaretLeftIcon,
  CaretRightIcon,
  HeadphonesIcon,
} from '@phosphor-icons/react'
import {
  connectBridge,
  postParam,
  type LevelFrame,
  type SpectrumFrame,
  type StereoImageFrame,
} from './bridge'
import {
  BAND_COUNT,
  BAND_NAMES,
  DEFAULT_PARAMS,
  DEFAULT_WIDTH,
  MAX_OUTPUT_DB,
  MAX_WIDTH,
  MIN_OUTPUT_DB,
  SOLO_NONE,
  cloneParams,
  crossoverId,
  describeWidth,
  formatDb,
  formatHz,
  formatWidth,
  sortedCrossovers,
  widthId,
  type ImagerParams,
} from './lib/params'
import { FACTORY_PRESETS, matchingPresetIndex, postAllParams } from './lib/presets'
import { BandDisplay } from './components/BandDisplay'
import { IconButton, Pill, PowerButton } from './components/Controls'
import { Knob } from './components/Knob'
import { CorrelationMeter, LevelMeter, isMeasurable } from './components/Meters'
import { PresetMenu } from './components/PresetMenu'
import { Vectorscope } from './components/Vectorscope'

const NO_LEVELS: LevelFrame = { inPeak: 0, inRms: 0, outPeak: 0, outRms: 0 }

function App() {
  const [params, setParams] = useState<ImagerParams>(() => cloneParams(DEFAULT_PARAMS))
  const [connected, setConnected] = useState(false)
  const [preset, setPreset] = useState<number | null>(0)
  const [presetOpen, setPresetOpen] = useState(false)
  // Correlation readouts re-render at the telemetry rate (~30 Hz), which is
  // what they are for; the scope and spectrum paint from refs instead.
  const [image, setImage] = useState<StereoImageFrame | null>(null)
  const [levels, setLevels] = useState<LevelFrame>(NO_LEVELS)
  const spectrum = useRef<SpectrumFrame | null>(null)
  const scope = useRef<StereoImageFrame | null>(null)
  const presetAnchor = useRef<HTMLButtonElement | null>(null)

  useEffect(
    () =>
      connectBridge(
        (nativeParams) => {
          setParams(nativeParams)
          setPreset(matchingPresetIndex(nativeParams))
          setPresetOpen(false)
        },
        (isConnected) => {
          setConnected(isConnected)
          if (!isConnected) {
            spectrum.current = null
            scope.current = null
            setImage(null)
            setLevels(NO_LEVELS)
          }
        },
        {
          onSpectrum: (frame) => {
            spectrum.current = frame
          },
          onStereoImage: (frame) => {
            scope.current = frame
            setImage(frame)
          },
          onLevels: setLevels,
        },
      ),
    [],
  )

  const setWidth = useCallback((band: number, width: number) => {
    setParams((current) => {
      if (current.width[band] === width) return current
      postParam(widthId(band), width)
      const next = [...current.width]
      next[band] = width
      return { ...current, width: next }
    })
    setPreset(null)
  }, [])

  const setCrossovers = useCallback((sorted: number[]) => {
    setParams((current) => {
      sorted.forEach((hz, index) => {
        if (current.crossoverHz[index] !== hz) postParam(crossoverId(index), hz)
      })
      return { ...current, crossoverHz: [...sorted] }
    })
    setPreset(null)
  }, [])

  const toggleSolo = useCallback((band: number) => {
    setParams((current) => {
      const next = current.soloBand === band ? SOLO_NONE : band
      postParam('soloBand', next)
      return { ...current, soloBand: next }
    })
  }, [])

  const clearSolo = useCallback(() => {
    setParams((current) => {
      if (current.soloBand === SOLO_NONE) return current
      postParam('soloBand', SOLO_NONE)
      return { ...current, soloBand: SOLO_NONE }
    })
  }, [])

  // Solo is an audition, never a state to leave behind: Escape ends it, and
  // so does the editor going away.
  const soloRef = useRef(params.soloBand)
  useEffect(() => {
    soloRef.current = params.soloBand
  }, [params.soloBand])
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape' && !presetOpen) clearSolo()
    }
    window.addEventListener('keydown', onKeyDown)
    return () => {
      window.removeEventListener('keydown', onKeyDown)
      if (soloRef.current !== SOLO_NONE) postParam('soloBand', SOLO_NONE)
    }
  }, [clearSolo, presetOpen])

  const setPower = useCallback((power: boolean) => {
    setParams((current) => ({ ...current, power }))
    postParam('power', power ? 1 : 0)
  }, [])

  const setOutput = useCallback((outputDb: number) => {
    setParams((current) => ({ ...current, outputDb }))
    postParam('outputDb', outputDb)
    setPreset(null)
  }, [])

  const loadPreset = useCallback((index: number) => {
    const wrapped = (index + FACTORY_PRESETS.length) % FACTORY_PRESETS.length
    const entry = FACTORY_PRESETS[wrapped]
    if (!entry) return
    const next = cloneParams(entry.params)
    setParams(next)
    setPreset(wrapped)
    postAllParams(next)
  }, [])

  const stepPreset = (delta: -1 | 1) => loadPreset((preset ?? (delta > 0 ? -1 : 0)) + delta)
  const presetLabel = preset === null ? 'Modified' : (FACTORY_PRESETS[preset]?.name ?? 'Modified')
  const sorted = sortedCrossovers(params)
  const edges = [20, ...sorted, 20_000]
  const outputLive = isMeasurable(levels.outRms)

  return (
    <main className="flex h-full w-full flex-col overflow-hidden bg-window">
      <header className="flex h-11 shrink-0 items-center gap-3 border-b border-line bg-bar px-3">
        <div className="flex min-w-0 items-center gap-2.5">
          <svg viewBox="0 0 24 24" className="h-5 w-5 shrink-0" aria-hidden="true">
            <rect x="1" y="1" width="22" height="22" rx="6" fill="var(--color-raised)" />
            <path
              d="M12 5v14M12 12 6.5 6.5M12 12l5.5-5.5M12 12l-5.5 5.5M12 12l5.5 5.5"
              fill="none"
              stroke="var(--color-accent)"
              strokeWidth="1.8"
              strokeLinecap="round"
            />
          </svg>
          <div className="flex items-baseline gap-2">
            <h1 className="text-[13px] font-bold tracking-[-0.01em]">IMAGER</h1>
            <span className="text-[11px] text-ink-3">Four-band stereo width</span>
          </div>
          <span
            className="h-1.5 w-1.5 rounded-full"
            style={{ background: connected ? 'var(--color-accent)' : 'var(--color-ink-4)' }}
            title={connected ? 'Linked to the insert' : 'Preview — no insert bound'}
            aria-label={connected ? 'Linked to the insert' : 'Preview, no insert bound'}
          />
        </div>

        <div className="flex flex-1 justify-center">
          <div className="flex items-center gap-1">
            <IconButton label="Previous preset" onClick={() => stepPreset(-1)}>
              <CaretLeftIcon size={13} weight="bold" />
            </IconButton>
            <button
              ref={presetAnchor}
              type="button"
              aria-haspopup="dialog"
              aria-expanded={presetOpen}
              onClick={() => setPresetOpen((open) => !open)}
              className={`flex h-7 w-56 cursor-pointer items-center gap-2 rounded-md border px-3 transition-colors duration-150 ${
                presetOpen ? 'border-line-hi bg-raised' : 'border-line bg-canvas hover:border-line-hi'
              }`}
            >
              <span className="min-w-0 flex-1 truncate text-left text-[12px] font-medium">{presetLabel}</span>
              <CaretDownIcon size={11} weight="bold" className="shrink-0 text-ink-3" />
            </button>
            <AnimatePresence>
              {presetOpen && (
                <PresetMenu
                  anchorRef={presetAnchor}
                  currentIndex={preset}
                  onLoad={loadPreset}
                  onClose={() => setPresetOpen(false)}
                />
              )}
            </AnimatePresence>
            <IconButton label="Next preset" onClick={() => stepPreset(1)}>
              <CaretRightIcon size={13} weight="bold" />
            </IconButton>
            <IconButton label="Reset to the default (every band at 100 %)" onClick={() => loadPreset(0)}>
              <ArrowCounterClockwiseIcon size={13} weight="bold" />
            </IconButton>
          </div>
        </div>

        <div className="flex items-center gap-1">
          <PowerButton on={params.power} onToggle={() => setPower(!params.power)} />
        </div>
      </header>

      <div className="flex min-h-0 flex-1 gap-2 p-3 pb-2">
        <section
          className={`relative min-h-[150px] min-w-0 flex-1 overflow-hidden rounded-lg border border-line bg-floor transition-[filter,opacity] duration-200 ${
            params.power ? '' : 'opacity-70 saturate-[0.3]'
          }`}
        >
          <BandDisplay
            params={params}
            bypassed={!params.power}
            spectrumRef={spectrum}
            onWidth={setWidth}
            onCrossovers={setCrossovers}
          />
          {!params.power && (
            <div className="pointer-events-none absolute inset-x-0 bottom-8 flex justify-center">
              <span className="rounded-md border border-line bg-bar/90 px-3 py-1.5 text-[11px] text-ink-2">
                Bypassed — Imager passes audio through unchanged
              </span>
            </div>
          )}
        </section>

        <aside className="flex w-[248px] shrink-0 flex-col gap-2">
          <section className="min-h-0 flex-1 overflow-hidden rounded-lg border border-line bg-floor">
            <Vectorscope frameRef={scope} />
          </section>
          <section className="flex shrink-0 flex-col gap-2.5 rounded-lg border border-line bg-panel p-3">
            <CorrelationMeter
              label="Output correlation"
              value={image?.correlation ?? 0}
              active={image !== null && outputLive}
            />
            <div className="flex flex-col gap-1.5">
              <LevelMeter label="In" peak={levels.inPeak} rms={levels.inRms} />
              <LevelMeter label="Out" peak={levels.outPeak} rms={levels.outRms} />
            </div>
          </section>
        </aside>
      </div>

      <section className="grid shrink-0 grid-cols-[repeat(4,minmax(0,1fr))_minmax(0,0.85fr)] gap-2 px-3 pb-3">
        {Array.from({ length: BAND_COUNT }, (_, band) => {
          const soloed = params.soloBand === band
          const muted = params.soloBand !== SOLO_NONE && !soloed
          const width = params.width[band]!
          return (
            <div
              key={band}
              className={`flex min-w-0 flex-col gap-2 rounded-lg border bg-panel px-3 pt-2.5 pb-3 transition-opacity duration-150 ${
                soloed ? 'border-accent/50' : 'border-line'
              } ${muted ? 'opacity-55' : ''}`}
            >
              <div className="flex items-center justify-between gap-2">
                <div className="flex min-w-0 flex-col">
                  <span className="truncate text-[12px] font-semibold">{BAND_NAMES[band]}</span>
                  <span className="num truncate text-[10px] text-ink-3">
                    {formatHz(edges[band]!)} – {formatHz(edges[band + 1]!)} Hz
                  </span>
                </div>
                <Pill
                  label="Solo"
                  on={soloed}
                  accent="var(--color-accent)"
                  title={soloed ? 'Stop soloing (Esc)' : `Hear only the ${BAND_NAMES[band]} band`}
                  onToggle={() => toggleSolo(band)}
                >
                  <HeadphonesIcon size={11} weight={soloed ? 'fill' : 'bold'} />
                </Pill>
              </div>
              <div className="flex items-center gap-3">
                <Knob
                  label="Width"
                  value={width}
                  min={0}
                  max={MAX_WIDTH}
                  step={1}
                  unit="%"
                  format={formatWidth}
                  defaultValue={DEFAULT_WIDTH}
                  originAtDefault
                  size={46}
                  onChange={(value) => setWidth(band, value)}
                />
                <div className="flex min-w-0 flex-1 flex-col gap-2">
                  <span
                    className={`text-[11px] font-medium ${width < 95 ? 'text-ink-2' : width > 105 ? 'text-accent-hi' : 'text-ink-3'}`}
                  >
                    {describeWidth(width)}
                  </span>
                  <CorrelationMeter
                    compact
                    label={`${BAND_NAMES[band]} correlation`}
                    value={image?.bandCorrelation[band] ?? 0}
                    active={image !== null && isMeasurable(image.bandLevel[band])}
                  />
                  <span className="num text-[10px] text-ink-4">
                    {image !== null && isMeasurable(image.bandLevel[band])
                      ? `Corr ${(image.bandCorrelation[band] ?? 0).toFixed(2)}`
                      : 'No signal'}
                  </span>
                </div>
              </div>
            </div>
          )
        })}

        <div className="flex min-w-0 flex-col items-center justify-center gap-1 rounded-lg border border-line bg-panel px-3 pt-2.5 pb-3">
          <Knob
            label="Output"
            value={params.outputDb}
            min={MIN_OUTPUT_DB}
            max={MAX_OUTPUT_DB}
            step={0.1}
            unit="dB"
            format={formatDb}
            defaultValue={0}
            originAtDefault
            size={50}
            onChange={setOutput}
          />
        </div>
      </section>
    </main>
  )
}

export default App
