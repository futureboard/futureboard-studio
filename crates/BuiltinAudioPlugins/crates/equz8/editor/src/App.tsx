import { useCallback, useEffect, useRef, useState } from 'react'
import { AnimatePresence } from 'motion/react'
import {
  ArrowCounterClockwiseIcon,
  CaretDownIcon,
  CaretLeftIcon,
  CaretRightIcon,
  ChartBarIcon,
  WaveSineIcon,
} from '@phosphor-icons/react'
import { SOLO_NONE, connectBridge, postParam, type Band, type EqParams, type SpectrumFrame } from './bridge'
import { DEFAULT_SAMPLE_RATE } from './lib/eq'
import {
  DEFAULT_PARAMS,
  FACTORY_PRESETS,
  cloneParams,
  matchingPresetIndex,
  postAllParams,
  postBandPatch,
} from './lib/presets'
import { IconButton, PowerButton } from './components/Controls'
import { BandEditor } from './components/BandEditor'
import { BandStrip } from './components/BandStrip'
import { PresetMenu } from './components/PresetMenu'
import { ResponseGraph } from './components/ResponseGraph'

/// How long "all bands in use" stays up after a double-click finds no free band.
const NOTICE_MS = 2200

function App() {
  const [params, setParams] = useState<EqParams>(DEFAULT_PARAMS)
  const [selected, setSelected] = useState(0)
  const [connected, setConnected] = useState(false)
  const [showBandCurves, setShowBandCurves] = useState(true)
  const [showSpectrum, setShowSpectrum] = useState(true)
  const [preset, setPreset] = useState<number | null>(0)
  const [presetOpen, setPresetOpen] = useState(false)
  const [sampleRate, setRate] = useState(DEFAULT_SAMPLE_RATE)
  const [notice, setNotice] = useState<string | null>(null)
  const spectrum = useRef<SpectrumFrame | null>(null)
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
          if (!isConnected) spectrum.current = null
        },
        (frame) => {
          spectrum.current = frame
        },
        (rate) => setRate(rate),
      ),
    [],
  )

  useEffect(() => {
    if (!notice) return
    const timer = window.setTimeout(() => setNotice(null), NOTICE_MS)
    return () => window.clearTimeout(timer)
  }, [notice])

  /// Any edit to a switched-off band switches it on: moving a band you cannot
  /// hear would be an edit with no result.
  const updateBand = useCallback((index: number, patch: Partial<Band>) => {
    setParams((current) => {
      const band = current.bands[index]
      if (!band) return current
      const effective =
        patch.active === undefined && !band.active && Object.keys(patch).length > 0
          ? { ...patch, active: true }
          : patch
      postBandPatch(index, effective)
      return {
        ...current,
        bands: current.bands.map((entry, bandIndex) => (bandIndex === index ? { ...entry, ...effective } : entry)),
      }
    })
    setPreset(null)
  }, [])

  const toggleSolo = useCallback((index: number) => {
    setParams((current) => {
      const next = current.soloBand === index ? SOLO_NONE : index
      postParam('soloBand', next)
      return { ...current, soloBand: next }
    })
  }, [])

  const setSolo = useCallback((index: number) => {
    setParams((current) => {
      if (current.soloBand === index) return current
      postParam('soloBand', index)
      return { ...current, soloBand: index }
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

  const updateGlobal = useCallback((patch: Partial<Pick<EqParams, 'power' | 'outputDb' | 'mix'>>) => {
    setParams((current) => ({ ...current, ...patch }))
    setPreset(null)
    if (patch.power !== undefined) postParam('power', patch.power ? 1 : 0)
    if (patch.outputDb !== undefined) postParam('outputDb', patch.outputDb)
    if (patch.mix !== undefined) postParam('mix', patch.mix)
  }, [])

  /// Double-click on empty graph: the first switched-off band becomes a bell
  /// right there. With all eight in use there is nothing to add, and the
  /// graph says so rather than silently moving a band the user set up.
  const addBand = useCallback(
    (freq: number, gainDb: number) => {
      const free = params.bands.findIndex((band) => !band.active)
      if (free < 0) {
        setNotice('All 8 bands are in use — switch one off to add another')
        return
      }
      updateBand(free, { active: true, bandType: 'bell', freq, gainDb, q: 1, dynamic: false })
      setSelected(free)
    },
    [params.bands, updateBand],
  )

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

  const band = params.bands[selected]
  const defaults = DEFAULT_PARAMS.bands[selected]
  if (!band || !defaults) return null

  const presetLabel = preset === null ? 'Modified' : (FACTORY_PRESETS[preset]?.name ?? 'Modified')
  const anyActive = params.bands.some((entry) => entry.active)

  return (
    <main className="flex h-full w-full flex-col overflow-hidden bg-window">
      <header className="flex h-11 shrink-0 items-center gap-3 border-b border-line bg-bar px-3">
        <div className="flex min-w-0 items-center gap-2.5">
          <svg viewBox="0 0 24 24" className="h-5 w-5 shrink-0" aria-hidden="true">
            <rect x="1" y="1" width="22" height="22" rx="6" fill="var(--color-raised)" />
            <path
              d="M4 15c3 0 3-7 6-7s3 9 6 9 2-5 4-5"
              fill="none"
              stroke="var(--color-accent)"
              strokeWidth="2"
              strokeLinecap="round"
            />
          </svg>
          <div className="flex items-baseline gap-2">
            <h1 className="text-[13px] font-bold tracking-[-0.01em]">EQUZ8</h1>
            <span className="text-[11px] text-ink-3">Dynamic EQ</span>
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
              className={`flex h-7 w-60 cursor-pointer items-center gap-2 rounded-md border px-3 transition-colors duration-150 ${
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
            <IconButton label="Reset to the default (all bands off)" onClick={() => loadPreset(0)}>
              <ArrowCounterClockwiseIcon size={13} weight="bold" />
            </IconButton>
          </div>
        </div>

        <div className="flex items-center gap-1">
          <IconButton
            label={showSpectrum ? 'Hide the analyser' : 'Show the analyser'}
            active={showSpectrum}
            onClick={() => setShowSpectrum((value) => !value)}
          >
            <ChartBarIcon size={14} weight={showSpectrum ? 'fill' : 'bold'} />
          </IconButton>
          <IconButton
            label={showBandCurves ? "Hide each band's curve" : "Show each band's curve"}
            active={showBandCurves}
            onClick={() => setShowBandCurves((value) => !value)}
          >
            <WaveSineIcon size={14} weight={showBandCurves ? 'fill' : 'bold'} />
          </IconButton>
          <div aria-hidden className="mx-1.5 h-4 w-px bg-line-hi" />
          <PowerButton on={params.power} onToggle={() => updateGlobal({ power: !params.power })} />
        </div>
      </header>

      <div className="flex min-h-0 flex-1 flex-col gap-2 p-3">
        <section
          className={`relative min-h-[160px] flex-1 overflow-hidden rounded-lg border border-line bg-floor transition-[filter,opacity] duration-200 ${
            params.power ? '' : 'opacity-70 saturate-[0.3]'
          }`}
        >
          <ResponseGraph
            sampleRate={sampleRate}
            bands={params.bands}
            selected={selected}
            bypassed={!params.power}
            showBandCurves={showBandCurves}
            showSpectrum={showSpectrum}
            spectrumRef={spectrum}
            soloBand={params.soloBand}
            onSelect={setSelected}
            onBandChange={updateBand}
            onAddBand={addBand}
            onToggleSolo={toggleSolo}
            onSetSolo={setSolo}
          />
          {(!anyActive || notice || !params.power) && (
            <div className="pointer-events-none absolute inset-x-0 bottom-7 z-[2] flex justify-center">
              <span className="rounded-md border border-line bg-bar/90 px-3 py-1.5 text-[11px] text-ink-2 backdrop-blur-sm">
                {!params.power
                  ? 'Bypassed — the EQ passes audio through unchanged'
                  : (notice ?? 'Double-click the graph to add a band, or drag a numbered node')}
              </span>
            </div>
          )}
        </section>

        <BandStrip
          bands={params.bands}
          selected={selected}
          soloBand={params.soloBand}
          onSelect={setSelected}
          onToggle={(index) => updateBand(index, { active: !params.bands[index]?.active })}
        />

        <BandEditor
          band={band}
          defaultBand={defaults}
          selected={selected}
          outputDb={params.outputDb}
          mix={params.mix}
          soloed={params.soloBand === selected}
          onBandChange={(patch) => updateBand(selected, patch)}
          onGlobalChange={updateGlobal}
          onToggleSolo={() => toggleSolo(selected)}
        />
      </div>
    </main>
  )
}

export default App
