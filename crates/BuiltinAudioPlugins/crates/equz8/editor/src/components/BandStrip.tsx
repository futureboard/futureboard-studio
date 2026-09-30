import type { CSSProperties } from 'react'
import type { Band } from '../bridge'
import { BAND_COLORS, bandHasGain, filterKind, formatFrequency, formatGain } from '../lib/eq'

export type BandStripProps = {
  bands: Band[]
  selected: number
  soloBand: number
  onSelect: (index: number) => void
  onToggle: (index: number) => void
}

/// All eight bands at a glance: shape, frequency and gain, which are on,
/// which is dynamic and which is being listened to. The cell is the band's
/// selector; its lamp switches it on and off.
export function BandStrip({ bands, selected, soloBand, onSelect, onToggle }: BandStripProps) {
  return (
    <div className="grid grid-cols-8 gap-1" role="tablist" aria-label="Bands">
      {bands.map((band, index) => {
        const color = BAND_COLORS[index] ?? 'var(--color-accent)'
        const isSelected = selected === index
        const kind = filterKind(band.bandType)
        return (
          <div
            key={index}
            role="tab"
            aria-selected={isSelected}
            tabIndex={isSelected ? 0 : -1}
            title={`Band ${index + 1}: ${kind.label} — click to edit, double-click to switch ${
              band.active ? 'off' : 'on'
            }`}
            onClick={() => onSelect(index)}
            onDoubleClick={() => onToggle(index)}
            onKeyDown={(event) => {
              if (event.key === 'ArrowRight') onSelect((index + 1) % bands.length)
              if (event.key === 'ArrowLeft') onSelect((index + bands.length - 1) % bands.length)
              if (event.key === ' ' || event.key === 'Enter') {
                event.preventDefault()
                onToggle(index)
              }
            }}
            className="relative flex min-w-0 cursor-pointer flex-col gap-0.5 overflow-hidden rounded-md border px-2 py-1.5 transition-colors duration-150"
            style={
              {
                borderColor: isSelected
                  ? `color-mix(in srgb, ${color} 55%, transparent)`
                  : 'var(--color-line)',
                background: isSelected
                  ? `color-mix(in srgb, ${color} 12%, var(--color-panel))`
                  : 'var(--color-panel)',
                opacity: band.active ? 1 : 0.55,
              } as CSSProperties
            }
          >
            {/* The band's colour, as a bar along the top — the same colour as its node. */}
            <span
              aria-hidden
              className="absolute inset-x-0 top-0 h-[2px]"
              style={{ background: band.active ? color : 'transparent' }}
            />
            <div className="flex items-center gap-1.5">
              <span className="num text-[11px] font-bold" style={{ color }}>
                {index + 1}
              </span>
              <svg viewBox="0 0 32 20" className="h-3 w-5 shrink-0" aria-hidden="true">
                <path
                  d={kind.glyph}
                  fill="none"
                  stroke={band.active ? color : 'var(--color-ink-4)'}
                  strokeWidth="2"
                  strokeLinecap="round"
                />
              </svg>
              <span className="min-w-0 flex-1" />
              {band.dynamic && bandHasGain(band.bandType) && (
                <span
                  className="rounded px-1 text-[8.5px] font-bold"
                  style={{ color, background: `color-mix(in srgb, ${color} 18%, transparent)` }}
                  title="Dynamic"
                >
                  D
                </span>
              )}
              {soloBand === index && (
                <span className="text-[8.5px] font-bold text-warn" title="Listening to this band alone">
                  S
                </span>
              )}
              <button
                type="button"
                role="switch"
                aria-checked={band.active}
                aria-label={`Band ${index + 1} ${band.active ? 'on' : 'off'}`}
                title={band.active ? 'Switch band off' : 'Switch band on'}
                onClick={(event) => {
                  event.stopPropagation()
                  onToggle(index)
                }}
                onDoubleClick={(event) => event.stopPropagation()}
                className="grid h-4 w-4 shrink-0 cursor-pointer place-items-center rounded-full"
              >
                <span
                  className="h-2 w-2 rounded-full border"
                  style={{
                    borderColor: band.active ? color : 'var(--color-ink-4)',
                    background: band.active ? color : 'transparent',
                  }}
                />
              </button>
            </div>
            <div className="num flex items-baseline gap-1 text-[10.5px]">
              <span className="text-ink">{formatFrequency(band.freq)}</span>
              <span className="text-ink-4">Hz</span>
              <span className="min-w-0 flex-1" />
              <span className="text-ink-2">
                {bandHasGain(band.bandType) ? `${formatGain(band.gainDb)}` : kind.short}
              </span>
            </div>
          </div>
        )
      })}
    </div>
  )
}
