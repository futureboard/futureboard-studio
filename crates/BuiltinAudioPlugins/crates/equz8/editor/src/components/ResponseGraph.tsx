import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type PointerEvent as ReactPointerEvent,
  type RefObject,
  type WheelEvent as ReactWheelEvent,
} from 'react'
import type { Band, SpectrumFrame } from '../bridge'
import { SpectrumLayer } from './SpectrumLayer'
import {
  BAND_COLORS,
  GAIN_RANGE,
  GRID_FREQUENCIES,
  GRID_GAINS,
  LABELLED_FREQUENCIES,
  MAX_FREQ,
  MIN_FREQ,
  bandCurveAreaPath,
  bandCurvePath,
  bandHasGain,
  clamp,
  filterKind,
  formatFrequency,
  formatGain,
  formatQ,
  frequencyToX,
  gainToY,
  scaleQ,
  sumCurveAreaPath,
  sumCurvePath,
  sumDbAt,
  xToFrequency,
  yToGain,
} from '../lib/eq'

const TAG_WIDTH = 150
const TAG_HEIGHT = 24

/// One wheel notch in `deltaY` units, matching a conventional mouse detent.
const WHEEL_NOTCH = 100
/// Q multiplier per wheel notch: the same perceived step anywhere in the range.
const Q_WHEEL_COARSE = 1.15
const Q_WHEEL_FINE = 1.03

/// Room kept free under the curve area for the frequency labels.
const AXIS_BOTTOM = 18

type Size = { width: number; height: number }

export type ResponseGraphProps = {
  bands: Band[]
  selected: number
  bypassed: boolean
  showBandCurves: boolean
  showSpectrum: boolean
  /// Live handle on the analyser frame — see [`SpectrumLayer`].
  spectrumRef: RefObject<SpectrumFrame | null>
  /// Band auditioned alone, or `SOLO_NONE`.
  soloBand: number
  /// The rate the host runs at; the curve is computed against it.
  sampleRate: number
  onSelect: (index: number) => void
  onBandChange: (index: number, patch: Partial<Band>) => void
  /// Double-click on empty graph: switch on a free band there.
  onAddBand: (frequency: number, gainDb: number) => void
  onToggleSolo: (index: number) => void
  onSetSolo: (index: number) => void
}

export function ResponseGraph({
  bands,
  selected,
  bypassed,
  showBandCurves,
  showSpectrum,
  spectrumRef,
  soloBand,
  sampleRate,
  onSelect,
  onBandChange,
  onAddBand,
  onToggleSolo,
  onSetSolo,
}: ResponseGraphProps) {
  const svgRef = useRef<SVGSVGElement>(null)
  const dragging = useRef<number | null>(null)
  /// Solo state to put back when a right-button audition ends, or `null` when
  /// the gesture in progress is not an audition.
  const auditionRestore = useRef<number | null>(null)
  const [size, setSize] = useState<Size>({ width: 960, height: 420 })
  const [cursor, setCursor] = useState<{ x: number; y: number } | null>(null)
  const [dragged, setDragged] = useState<number | null>(null)

  useEffect(() => {
    const node = svgRef.current
    if (!node) return
    const measure = () => {
      const rect = node.getBoundingClientRect()
      if (rect.width <= 0 || rect.height <= 0) return
      setSize((current) =>
        Math.abs(current.width - rect.width) < 0.5 && Math.abs(current.height - rect.height) < 0.5
          ? current
          : { width: rect.width, height: rect.height },
      )
    }
    measure()
    const observer = new ResizeObserver(measure)
    observer.observe(node)
    return () => observer.disconnect()
  }, [])

  const { width } = size
  // The gain axis stops above the frequency labels, so the lowest gridline
  // and the labels never overlap.
  const height = Math.max(40, size.height - AXIS_BOTTOM)

  const sumPath = useMemo(
    () => sumCurvePath(bands, width, height, sampleRate),
    [bands, height, width, sampleRate],
  )
  const sumArea = useMemo(
    () => sumCurveAreaPath(bands, width, height, sampleRate),
    [bands, height, width, sampleRate],
  )
  const otherBandPaths = useMemo(
    () =>
      showBandCurves
        ? bands.map((band, index) =>
            band.active && index !== selected ? bandCurvePath(band, width, height, sampleRate) : null,
          )
        : null,
    [bands, height, selected, showBandCurves, width, sampleRate],
  )
  const selectedBand = bands[selected]
  const selectedPaths = useMemo(() => {
    if (!selectedBand?.active) return null
    return {
      line: bandCurvePath(selectedBand, width, height, sampleRate),
      area: bandCurveAreaPath(selectedBand, width, height, sampleRate),
    }
  }, [selectedBand, height, width, sampleRate])
  /// The fully-triggered dynamic gain (gain + range), drawn as a ghost.
  const dynamicExtentPath = useMemo(() => {
    const band = selectedBand
    if (!band?.active || !band.dynamic || !bandHasGain(band.bandType) || Math.abs(band.rangeDb) < 0.01) {
      return null
    }
    return bandCurvePath({ ...band, gainDb: band.gainDb + band.rangeDb }, width, height, sampleRate)
  }, [selectedBand, height, width, sampleRate])

  /// Client coordinates in the viewBox space everything is drawn in.
  const toGraphPoint = useCallback(
    (clientX: number, clientY: number) => {
      const rect = svgRef.current?.getBoundingClientRect()
      if (!rect || rect.width <= 0 || rect.height <= 0) return null
      return {
        x: ((clientX - rect.left) / rect.width) * width,
        y: ((clientY - rect.top) / rect.height) * size.height,
      }
    },
    [size.height, width],
  )

  const dragBand = useCallback(
    (index: number, clientX: number, clientY: number) => {
      const point = toGraphPoint(clientX, clientY)
      const band = bands[index]
      if (!point || !band) return
      const patch: Partial<Band> = { freq: clamp(xToFrequency(point.x, width), MIN_FREQ, MAX_FREQ) }
      if (bandHasGain(band.bandType)) {
        patch.gainDb = clamp(yToGain(point.y, height), -GAIN_RANGE, GAIN_RANGE)
      }
      onBandChange(index, patch)
    },
    [bands, height, onBandChange, toGraphPoint, width],
  )

  const onNodeWheel = (event: ReactWheelEvent<SVGGElement>, index: number) => {
    event.preventDefault()
    const band = bands[index]
    if (!band) return
    const notches = clamp(event.deltaY / WHEEL_NOTCH, -4, 4)
    const perNotch = event.shiftKey ? Q_WHEEL_FINE : Q_WHEEL_COARSE
    onBandChange(index, { q: scaleQ(band.q, Math.pow(perNotch, -notches)) })
  }

  /// End any drag, releasing a momentary audition. Every teardown path comes
  /// here: a missed release would leave the EQ stuck soloing one band.
  const endGesture = useCallback(() => {
    dragging.current = null
    setDragged(null)
    if (auditionRestore.current !== null) {
      onSetSolo(auditionRestore.current)
      auditionRestore.current = null
    }
  }, [onSetSolo])

  // The drag runs from the window: a right-button gesture loses pointer
  // capture when Blink fires `pointercancel` for the context menu, and a
  // release outside the window must still end the audition.
  useEffect(() => {
    const onWindowMove = (event: PointerEvent) => {
      if (dragging.current === null) return
      dragBand(dragging.current, event.clientX, event.clientY)
      setCursor(null)
    }
    const onWindowUp = () => {
      if (dragging.current !== null || auditionRestore.current !== null) endGesture()
    }
    window.addEventListener('pointermove', onWindowMove)
    window.addEventListener('pointerup', onWindowUp)
    window.addEventListener('blur', onWindowUp)
    return () => {
      window.removeEventListener('pointermove', onWindowMove)
      window.removeEventListener('pointerup', onWindowUp)
      window.removeEventListener('blur', onWindowUp)
    }
  }, [dragBand, endGesture])

  const cursorFreq = cursor ? xToFrequency(cursor.x, width) : null
  const cursorDb = cursorFreq !== null ? sumDbAt(bands, cursorFreq, sampleRate) : null
  const zeroY = gainToY(0, height)

  return (
    <div className="graph">
      <SpectrumLayer frameRef={spectrumRef} visible={showSpectrum && !bypassed} />
      <svg
        ref={svgRef}
        className={`response${bypassed ? ' is-bypassed' : ''}`}
        viewBox={`0 0 ${width} ${size.height}`}
        preserveAspectRatio="none"
        onPointerMove={(event: ReactPointerEvent<SVGSVGElement>) => {
          if (dragging.current !== null) return
          setCursor(toGraphPoint(event.clientX, event.clientY))
        }}
        onPointerLeave={() => setCursor(null)}
        onContextMenu={(event) => event.preventDefault()}
        onPointerUp={(event) => {
          endGesture()
          if (event.currentTarget.hasPointerCapture(event.pointerId)) {
            event.currentTarget.releasePointerCapture(event.pointerId)
          }
        }}
        // Not `endGesture`: Blink fires `pointercancel` when it opens a context
        // menu, which would end the right-button drag the instant it starts.
        onPointerCancel={() => setCursor(null)}
        onDoubleClick={(event) => {
          // Only empty graph: a node's own double-click resets it.
          if ((event.target as Element).closest('.node')) return
          const point = toGraphPoint(event.clientX, event.clientY)
          if (!point) return
          onAddBand(
            clamp(xToFrequency(point.x, width), MIN_FREQ, MAX_FREQ),
            clamp(yToGain(point.y, height), -GAIN_RANGE, GAIN_RANGE),
          )
        }}
      >
        <defs>
          <linearGradient id="sum-fill" x1="0" x2="0" y1="0" y2="1">
            <stop offset="0" stopColor="var(--color-accent)" stopOpacity="0.28" />
            <stop offset={clamp(zeroY / height, 0.01, 0.99)} stopColor="var(--color-accent)" stopOpacity="0.04" />
            <stop offset="1" stopColor="var(--color-accent)" stopOpacity="0.22" />
          </linearGradient>
          <filter id="sum-glow" x="-4%" y="-30%" width="108%" height="160%">
            <feGaussianBlur stdDeviation="3" />
          </filter>
        </defs>

        <g>
          {GRID_FREQUENCIES.map((frequency) => {
            const x = frequencyToX(frequency, width)
            return (
              <line
                key={frequency}
                x1={x}
                x2={x}
                y1={0}
                y2={height}
                className={LABELLED_FREQUENCIES.includes(frequency) ? 'grid-line is-major' : 'grid-line'}
              />
            )
          })}
          {GRID_GAINS.map((gain) => (
            <line
              key={gain}
              x1={0}
              x2={width}
              y1={gainToY(gain, height)}
              y2={gainToY(gain, height)}
              className={gain === 0 ? 'grid-line is-zero' : 'grid-line'}
            />
          ))}
        </g>

        <g>
          {LABELLED_FREQUENCIES.map((frequency) => (
            <text key={frequency} x={frequencyToX(frequency, width)} y={height + 13} className="axis-freq">
              {formatFrequency(frequency)}
            </text>
          ))}
          {GRID_GAINS.map((gain) => (
            <text key={gain} x={6} y={gainToY(gain, height) - 4} className="axis-gain">
              {gain > 0 ? `+${gain}` : gain}
            </text>
          ))}
        </g>

        {otherBandPaths?.map((path, index) =>
          path ? (
            <path
              key={index}
              d={path}
              className="band-curve"
              style={{ '--band': BAND_COLORS[index] } as CSSProperties}
            />
          ) : null,
        )}

        {selectedPaths && (
          <g style={{ '--band': BAND_COLORS[selected] } as CSSProperties}>
            <path d={selectedPaths.area} className="band-area" />
            {dynamicExtentPath && <path d={dynamicExtentPath} className="band-curve is-dynamic-extent" />}
            <path d={selectedPaths.line} className="band-curve is-selected" />
          </g>
        )}

        <path className="sum-area" d={sumArea} />
        <path d={sumPath} className="sum-glow" filter="url(#sum-glow)" />
        <path d={sumPath} className="sum-line" />

        {cursor && cursorFreq !== null && cursorDb !== null && dragging.current === null && (
          <g pointerEvents="none">
            <line x1={cursor.x} x2={cursor.x} y1={0} y2={height} className="cursor-line" />
            <text
              x={clamp(cursor.x + 8, 8, width - 110)}
              y={16}
              className="axis-freq"
              style={{ textAnchor: 'start', fill: 'var(--color-ink-2)' }}
            >
              {formatFrequency(cursorFreq)} Hz · {formatGain(cursorDb)} dB
            </text>
          </g>
        )}

        {bands.map((band, index) => {
          const x = frequencyToX(band.freq, width)
          const y = gainToY(bandHasGain(band.bandType) ? band.gainDb : 0, height)
          const isSelected = selected === index
          const tagX = clamp(x, TAG_WIDTH / 2 + 6, Math.max(TAG_WIDTH / 2 + 6, width - TAG_WIDTH / 2 - 6)) - x
          const tagY = y < TAG_HEIGHT + 34 ? 32 : -32
          return (
            <g
              key={index}
              className={`node${isSelected ? ' is-selected' : ''}${band.active ? '' : ' is-off'}${
                soloBand === index ? ' is-soloed' : ''
              }`}
              transform={`translate(${x} ${y})`}
              style={{ '--band': BAND_COLORS[index] } as CSSProperties}
              role="slider"
              tabIndex={0}
              aria-label={`Band ${index + 1} ${filterKind(band.bandType).label}`}
              aria-valuemin={MIN_FREQ}
              aria-valuemax={MAX_FREQ}
              aria-valuenow={Math.round(band.freq)}
              aria-valuetext={`${formatFrequency(band.freq)} hertz, ${formatGain(band.gainDb)} decibel`}
              onPointerDown={(event) => {
                onSelect(index)
                // Alt-click latches the audition on or off.
                if (event.altKey && event.button === 0) {
                  event.preventDefault()
                  onToggleSolo(index)
                  return
                }
                // Right-button hold: listen to this band alone while it moves;
                // the release puts back whatever solo state was there.
                if (event.button === 2) {
                  event.preventDefault()
                  auditionRestore.current = soloBand
                  onSetSolo(index)
                }
                dragging.current = index
                setDragged(index)
                setCursor(null)
                svgRef.current?.setPointerCapture(event.pointerId)
              }}
              onContextMenu={(event) => event.preventDefault()}
              onDoubleClick={() => onBandChange(index, { gainDb: 0, q: 1 })}
              onWheel={(event) => onNodeWheel(event, index)}
              onKeyDown={(event) => {
                const factor = event.shiftKey ? 1.01 : 1.05
                if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') {
                  event.preventDefault()
                  onBandChange(index, {
                    freq: clamp(band.freq * (event.key === 'ArrowRight' ? factor : 1 / factor), MIN_FREQ, MAX_FREQ),
                  })
                } else if (event.key === 'ArrowUp' || event.key === 'ArrowDown') {
                  event.preventDefault()
                  if (!bandHasGain(band.bandType)) return
                  onBandChange(index, {
                    gainDb: clamp(band.gainDb + (event.key === 'ArrowUp' ? 0.5 : -0.5), -GAIN_RANGE, GAIN_RANGE),
                  })
                } else if (event.key === 'Delete' || event.key === 'Backspace') {
                  event.preventDefault()
                  onBandChange(index, { active: false })
                }
              }}
            >
              <title>
                {`Band ${index + 1} — drag to move${band.active ? '' : ' (turns it on)'}, wheel for Q, ` +
                  'hold right-button to listen while moving, Alt-click to keep listening, Delete to switch off'}
              </title>
              <circle className="node-halo" r={isSelected ? 18 : 15} />
              {isSelected && <circle className="node-ring" r={12} />}
              <circle className="node-dot" r={isSelected ? 9 : 8} />
              <text textAnchor="middle" dominantBaseline="central">
                {index + 1}
              </text>
              {dragged === index && (
                <g className="node-tag" transform={`translate(${tagX} ${tagY})`} pointerEvents="none">
                  <rect x={-TAG_WIDTH / 2} y={-TAG_HEIGHT / 2} width={TAG_WIDTH} height={TAG_HEIGHT} rx="5" />
                  <text textAnchor="middle" y="4">
                    {formatFrequency(band.freq)} Hz
                    {bandHasGain(band.bandType) ? ` · ${formatGain(band.gainDb)} dB` : ''} · Q {formatQ(band.q)}
                  </text>
                </g>
              )}
            </g>
          )
        })}
      </svg>
    </div>
  )
}
