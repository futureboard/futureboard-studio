import { useEffect, useRef, useState } from 'react'
import { HeadphonesIcon, ProhibitIcon } from '@phosphor-icons/react'
import { connectBridge, postParam, type LevelFrame, type SpectrumFrame } from './bridge'
import {
  BAND_COUNT,
  BAND_NAMES,
  DEFAULT_BANDS,
  DEFAULT_PARAMS,
  MAX_ATTACK_MS,
  MAX_KNEE_DB,
  MAX_MAKEUP_DB,
  MAX_OUTPUT_DB,
  MAX_RATIO,
  MAX_RELEASE_MS,
  MAX_SIDECHAIN_HZ,
  MAX_THRESHOLD_DB,
  MIN_ATTACK_MS,
  MIN_MAKEUP_DB,
  MIN_OUTPUT_DB,
  MIN_RATIO,
  MIN_RELEASE_MS,
  MIN_THRESHOLD_DB,
  SOLO_NONE,
  bandParamId,
  cloneParams,
  crossoverId,
  formatDb,
  formatHz,
  formatMs,
  formatPercent,
  formatRatio,
  formatSidechain,
  formatThreshold,
  logTravel,
  sortedCrossovers,
  type Band,
  type BandField,
  type CompressorParams,
  type Mode,
} from './lib/params'
import { BandDisplay } from './components/BandDisplay'
import { Pill, PowerButton, Segmented } from './components/Controls'
import { Knob } from './components/Knob'
import { LevelMeter, ReductionMeter } from './components/Meters'
import { TransferCurve } from './components/TransferCurve'

const NO_LEVELS: LevelFrame = { inPeak: 0, inRms: 0, outPeak: 0, outRms: 0, gainReductionDb: 0 }
const NO_REDUCTION: number[] = Array.from({ length: BAND_COUNT }, () => 0)

const ATTACK_TRAVEL = logTravel(MIN_ATTACK_MS, MAX_ATTACK_MS)
const RELEASE_TRAVEL = logTravel(MIN_RELEASE_MS, MAX_RELEASE_MS)
const RATIO_TRAVEL = logTravel(MIN_RATIO, MAX_RATIO)
const SIDECHAIN_TRAVEL = logTravel(10, MAX_SIDECHAIN_HZ)

const MODES = [
  { value: 'single', label: 'Single', title: 'One compressor over the whole signal' },
  { value: 'multi', label: 'Multi', title: 'Four crossover bands, each with its own compressor' },
] as const satisfies readonly { value: Mode; label: string; title: string }[]

/// Scalar params whose wire id is the field name.
type ScalarKey =
  | 'thresholdDb'
  | 'ratio'
  | 'attackMs'
  | 'releaseMs'
  | 'makeupDb'
  | 'sidechainHpfHz'
  | 'kneeDb'
  | 'mix'
  | 'outputDb'

const BAND_KEYS: Record<Exclude<BandField, 'Bypass'>, keyof Omit<Band, 'bypass'>> = {
  ThresholdDb: 'thresholdDb',
  Ratio: 'ratio',
  AttackMs: 'attackMs',
  ReleaseMs: 'releaseMs',
  MakeupDb: 'makeupDb',
}

function App() {
  const [params, setParams] = useState<CompressorParams>(() => cloneParams(DEFAULT_PARAMS))
  const [connected, setConnected] = useState(false)
  // Meters re-render at the telemetry rate (~30 Hz), which is what they are
  // for; the spectrum paints from a ref instead.
  const [levels, setLevels] = useState<LevelFrame>(NO_LEVELS)
  const [bandReduction, setBandReduction] = useState<number[]>(NO_REDUCTION)
  const spectrum = useRef<SpectrumFrame | null>(null)

  useEffect(
    () =>
      connectBridge(
        (nativeParams) => setParams(nativeParams),
        (isConnected) => {
          setConnected(isConnected)
          if (!isConnected) {
            spectrum.current = null
            setLevels(NO_LEVELS)
            setBandReduction(NO_REDUCTION)
          }
        },
        {
          onSpectrum: (frame) => {
            spectrum.current = frame
          },
          onLevels: setLevels,
          onBandReduction: setBandReduction,
        },
      ),
    [],
  )

  const setScalar = (key: ScalarKey, value: number) => {
    setParams((current) => ({ ...current, [key]: value }))
    postParam(key, value)
  }

  const setBand = (band: number, field: Exclude<BandField, 'Bypass'>, value: number) => {
    const key = BAND_KEYS[field]
    setParams((current) => ({
      ...current,
      bands: current.bands.map((entry, index) => (index === band ? { ...entry, [key]: value } : entry)),
    }))
    postParam(bandParamId(band, field), value)
  }

  const toggleBypass = (band: number) => {
    const bypass = !params.bands[band]!.bypass
    setParams((current) => ({
      ...current,
      bands: current.bands.map((entry, index) => (index === band ? { ...entry, bypass } : entry)),
    }))
    postParam(bandParamId(band, 'Bypass'), bypass ? 1 : 0)
  }

  const setSolo = (soloBand: number) => {
    setParams((current) => ({ ...current, soloBand }))
    postParam('soloBand', soloBand)
  }

  const setCrossovers = (sorted: number[]) => {
    sorted.forEach((hz, index) => {
      if (params.crossoverHz[index] !== hz) postParam(crossoverId(index), hz)
    })
    setParams((current) => ({ ...current, crossoverHz: [...sorted] }))
  }

  const setMode = (mode: Mode) => {
    if (mode === params.mode) return
    setParams((current) => ({ ...current, mode }))
    postParam('mode', mode === 'multi' ? 1 : 0)
  }

  const setPower = (power: boolean) => {
    setParams((current) => ({ ...current, power }))
    postParam('power', power ? 1 : 0)
  }

  // Solo is an audition, never a state to leave behind: Escape ends it, and
  // so does the editor going away.
  const soloRef = useRef(params.soloBand)
  useEffect(() => {
    soloRef.current = params.soloBand
  }, [params.soloBand])
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== 'Escape' || soloRef.current === SOLO_NONE) return
      setParams((current) => ({ ...current, soloBand: SOLO_NONE }))
      postParam('soloBand', SOLO_NONE)
    }
    window.addEventListener('keydown', onKeyDown)
    return () => {
      window.removeEventListener('keydown', onKeyDown)
      if (soloRef.current !== SOLO_NONE) postParam('soloBand', SOLO_NONE)
    }
  }, [])

  const multi = params.mode === 'multi'
  const sorted = sortedCrossovers(params)
  const edges = [20, ...sorted, 20_000]

  return (
    <main className="flex h-full w-full flex-col overflow-hidden bg-window">
      <header className="flex h-11 shrink-0 items-center gap-3 border-b border-line bg-bar px-3">
        <div className="flex min-w-0 flex-1 items-center gap-2.5">
          <svg viewBox="0 0 24 24" className="h-5 w-5 shrink-0" aria-hidden="true">
            <rect x="1" y="1" width="22" height="22" rx="6" fill="var(--color-raised)" />
            <path
              d="M5 18 L11 12 L19 8"
              fill="none"
              stroke="var(--color-accent)"
              strokeWidth="1.8"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
            <path d="M5 18 L19 4" fill="none" stroke="var(--color-ink-4)" strokeWidth="1.2" strokeDasharray="2 2" />
          </svg>
          <div className="flex min-w-0 items-baseline gap-2">
            <h1 className="text-[13px] font-bold tracking-[-0.01em]">COMPRESSOR</h1>
            <span className="truncate text-[11px] text-ink-3">
              {multi ? 'Four-band compression' : 'Single-band compression'}
            </span>
          </div>
          <span
            className="h-1.5 w-1.5 shrink-0 rounded-full"
            style={{ background: connected ? 'var(--color-accent)' : 'var(--color-ink-4)' }}
            title={connected ? 'Linked to the insert' : 'Preview — no insert bound'}
            aria-label={connected ? 'Linked to the insert' : 'Preview, no insert bound'}
          />
        </div>

        <Segmented label="Mode" value={params.mode} options={MODES} onChange={setMode} />

        <div className="flex flex-1 items-center justify-end gap-1">
          <PowerButton on={params.power} onToggle={() => setPower(!params.power)} />
        </div>
      </header>

      <div className="flex min-h-0 flex-1 gap-2 p-3 pb-2">
        <section
          className={`relative min-h-[150px] min-w-0 flex-1 overflow-hidden rounded-lg border border-line bg-floor transition-[filter,opacity] duration-200 ${
            params.power ? '' : 'opacity-70 saturate-[0.3]'
          }`}
        >
          {multi ? (
            <BandDisplay
              params={params}
              bypassed={!params.power}
              spectrumRef={spectrum}
              reductionDb={bandReduction}
              onCrossovers={setCrossovers}
            />
          ) : (
            <TransferCurve
              thresholdDb={params.thresholdDb}
              ratio={params.ratio}
              kneeDb={params.kneeDb}
              inputPeak={levels.inPeak}
              reductionDb={levels.gainReductionDb}
              live={params.power}
              onThreshold={(db) => setScalar('thresholdDb', db)}
            />
          )}
          {!params.power && (
            <div className="pointer-events-none absolute inset-x-0 bottom-8 flex justify-center">
              <span className="rounded-md border border-line bg-bar/90 px-3 py-1.5 text-[11px] text-ink-2">
                Bypassed — the Compressor passes audio through unchanged
              </span>
            </div>
          )}
        </section>

        <aside className="flex w-[248px] shrink-0 flex-col gap-2">
          <section className="flex flex-col gap-2 rounded-lg border border-line bg-panel p-3">
            <span className="cap">Levels</span>
            <ReductionMeter label="GR" reductionDb={levels.gainReductionDb} active={params.power} />
            <LevelMeter label="In" peak={levels.inPeak} rms={levels.inRms} />
            <LevelMeter label="Out" peak={levels.outPeak} rms={levels.outRms} />
          </section>
          <section className="flex min-h-0 flex-1 items-start justify-between gap-1 rounded-lg border border-line bg-panel px-2 pt-3 pb-2">
            <Knob
              label="Knee"
              value={params.kneeDb}
              min={0}
              max={MAX_KNEE_DB}
              step={0.1}
              unit="dB"
              format={(db) => db.toFixed(1)}
              defaultValue={DEFAULT_PARAMS.kneeDb}
              size={40}
              onChange={(value) => setScalar('kneeDb', value)}
            />
            <Knob
              label="Mix"
              value={params.mix}
              min={0}
              max={100}
              step={1}
              unit="%"
              format={formatPercent}
              defaultValue={DEFAULT_PARAMS.mix}
              size={40}
              onChange={(value) => setScalar('mix', value)}
            />
            <Knob
              label="Output"
              value={params.outputDb}
              min={MIN_OUTPUT_DB}
              max={MAX_OUTPUT_DB}
              step={0.1}
              unit="dB"
              format={formatDb}
              defaultValue={DEFAULT_PARAMS.outputDb}
              originAtDefault
              size={40}
              onChange={(value) => setScalar('outputDb', value)}
            />
          </section>
        </aside>
      </div>

      {multi ? (
        <section className="grid shrink-0 grid-cols-4 gap-2 px-3 pb-3">
          {Array.from({ length: BAND_COUNT }, (_, band) => (
            <BandCard
              key={band}
              band={band}
              values={params.bands[band]!}
              lowHz={edges[band]!}
              highHz={edges[band + 1]!}
              soloed={params.soloBand === band}
              muted={params.soloBand !== SOLO_NONE && params.soloBand !== band}
              reductionDb={bandReduction[band] ?? 0}
              live={params.power}
              onChange={(field, value) => setBand(band, field, value)}
              onBypass={() => toggleBypass(band)}
              onSolo={() => setSolo(params.soloBand === band ? SOLO_NONE : band)}
            />
          ))}
        </section>
      ) : (
        <section className="mx-3 mb-3 flex shrink-0 items-start justify-evenly gap-2 rounded-lg border border-line bg-panel px-3 pt-3 pb-2">
          <Knob
            label="Threshold"
            value={params.thresholdDb}
            min={MIN_THRESHOLD_DB}
            max={MAX_THRESHOLD_DB}
            step={0.1}
            unit="dB"
            format={formatThreshold}
            defaultValue={DEFAULT_PARAMS.thresholdDb}
            size={48}
            onChange={(value) => setScalar('thresholdDb', value)}
          />
          <Knob
            label="Ratio"
            value={params.ratio}
            min={MIN_RATIO}
            max={MAX_RATIO}
            step={0.1}
            unit=": 1"
            format={formatRatio}
            defaultValue={DEFAULT_PARAMS.ratio}
            {...RATIO_TRAVEL}
            size={48}
            onChange={(value) => setScalar('ratio', value)}
          />
          <Knob
            label="Attack"
            value={params.attackMs}
            min={MIN_ATTACK_MS}
            max={MAX_ATTACK_MS}
            step={0.1}
            unit="ms"
            format={formatMs}
            defaultValue={DEFAULT_PARAMS.attackMs}
            {...ATTACK_TRAVEL}
            size={48}
            onChange={(value) => setScalar('attackMs', value)}
          />
          <Knob
            label="Release"
            value={params.releaseMs}
            min={MIN_RELEASE_MS}
            max={MAX_RELEASE_MS}
            step={1}
            unit="ms"
            format={formatMs}
            defaultValue={DEFAULT_PARAMS.releaseMs}
            {...RELEASE_TRAVEL}
            size={48}
            onChange={(value) => setScalar('releaseMs', value)}
          />
          <Knob
            label="Makeup"
            value={params.makeupDb}
            min={MIN_MAKEUP_DB}
            max={MAX_MAKEUP_DB}
            step={0.1}
            unit="dB"
            format={formatDb}
            defaultValue={DEFAULT_PARAMS.makeupDb}
            originAtDefault
            size={48}
            onChange={(value) => setScalar('makeupDb', value)}
          />
          <Knob
            label="SC HPF"
            value={params.sidechainHpfHz}
            min={0}
            max={MAX_SIDECHAIN_HZ}
            step={1}
            unit={params.sidechainHpfHz > 20 ? 'Hz' : undefined}
            format={formatSidechain}
            defaultValue={DEFAULT_PARAMS.sidechainHpfHz}
            toProgress={(hz) => (hz <= 10 ? 0 : SIDECHAIN_TRAVEL.toProgress(hz))}
            fromProgress={(progress) => (progress <= 0.001 ? 0 : SIDECHAIN_TRAVEL.fromProgress(progress))}
            size={48}
            onChange={(value) => setScalar('sidechainHpfHz', value)}
          />
        </section>
      )}
    </main>
  )
}

function BandCard({
  band,
  values,
  lowHz,
  highHz,
  soloed,
  muted,
  reductionDb,
  live,
  onChange,
  onBypass,
  onSolo,
}: {
  band: number
  values: Band
  lowHz: number
  highHz: number
  soloed: boolean
  muted: boolean
  reductionDb: number
  live: boolean
  onChange: (field: Exclude<BandField, 'Bypass'>, value: number) => void
  onBypass: () => void
  onSolo: () => void
}) {
  const defaults = DEFAULT_BANDS[band]!
  const name = BAND_NAMES[band]
  return (
    <div
      className={`flex min-w-0 flex-col gap-2 rounded-lg border bg-panel px-2.5 pt-2.5 pb-2 transition-opacity duration-150 ${
        soloed ? 'border-warn/50' : 'border-line'
      } ${muted ? 'opacity-55' : ''}`}
    >
      <div className="flex items-center justify-between gap-1.5">
        <div className="flex min-w-0 flex-col">
          <span className="truncate text-[12px] font-semibold">{name}</span>
          <span className="num truncate text-[10px] text-ink-3">
            {formatHz(lowHz)} – {formatHz(highHz)} Hz
          </span>
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <Pill
            label="Byp"
            on={values.bypass}
            accent="var(--color-ink-2)"
            title={values.bypass ? `Compress the ${name} band again` : `Pass the ${name} band through uncompressed`}
            onToggle={onBypass}
          >
            <ProhibitIcon size={11} weight="bold" />
          </Pill>
          <Pill
            label="Solo"
            on={soloed}
            accent="var(--color-warn)"
            title={soloed ? 'Stop soloing (Esc)' : `Hear only the ${name} band`}
            onToggle={onSolo}
          >
            <HeadphonesIcon size={11} weight={soloed ? 'fill' : 'bold'} />
          </Pill>
        </div>
      </div>
      <ReductionMeter label="GR" reductionDb={reductionDb} active={live && !values.bypass} />
      <div className={`flex flex-col gap-1 ${values.bypass ? 'opacity-55' : ''}`}>
        <div className="flex items-start justify-between">
          <Knob
            label="Thresh"
            value={values.thresholdDb}
            min={MIN_THRESHOLD_DB}
            max={MAX_THRESHOLD_DB}
            step={0.1}
            unit="dB"
            format={formatThreshold}
            defaultValue={defaults.thresholdDb}
            size={38}
            onChange={(value) => onChange('ThresholdDb', value)}
          />
          <Knob
            label="Ratio"
            value={values.ratio}
            min={MIN_RATIO}
            max={MAX_RATIO}
            step={0.1}
            unit=": 1"
            format={formatRatio}
            defaultValue={defaults.ratio}
            {...RATIO_TRAVEL}
            size={38}
            onChange={(value) => onChange('Ratio', value)}
          />
          <Knob
            label="Makeup"
            value={values.makeupDb}
            min={MIN_MAKEUP_DB}
            max={MAX_MAKEUP_DB}
            step={0.1}
            unit="dB"
            format={formatDb}
            defaultValue={defaults.makeupDb}
            originAtDefault
            size={38}
            onChange={(value) => onChange('MakeupDb', value)}
          />
        </div>
        <div className="flex items-start justify-evenly">
          <Knob
            label="Attack"
            value={values.attackMs}
            min={MIN_ATTACK_MS}
            max={MAX_ATTACK_MS}
            step={0.1}
            unit="ms"
            format={formatMs}
            defaultValue={defaults.attackMs}
            {...ATTACK_TRAVEL}
            size={34}
            onChange={(value) => onChange('AttackMs', value)}
          />
          <Knob
            label="Release"
            value={values.releaseMs}
            min={MIN_RELEASE_MS}
            max={MAX_RELEASE_MS}
            step={1}
            unit="ms"
            format={formatMs}
            defaultValue={defaults.releaseMs}
            {...RELEASE_TRAVEL}
            size={34}
            onChange={(value) => onChange('ReleaseMs', value)}
          />
        </div>
      </div>
    </div>
  )
}

export default App
