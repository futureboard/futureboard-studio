import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { AnimatePresence } from 'motion/react'
import {
  ArrowCounterClockwiseIcon,
  CaretDownIcon,
  CaretLeftIcon,
  CaretRightIcon,
  CheckIcon,
  MagnifyingGlassIcon,
} from '@phosphor-icons/react'
import { IS_BROWSER_PREVIEW } from './bridge'
import { IconButton, PowerButton } from './Controls'
import { Popover } from './Popover'

export type HeaderProps = {
  name: string
  subtitle: string
  mark: ReactNode
  connected: boolean
  presets: readonly { name: string }[]
  /// Index of the factory preset the state matches, or `null` when edited.
  presetIndex: number | null
  onPreset: (index: number) => void
  power: boolean
  onPower: (on: boolean) => void
  /// Which insert this page is bound to, when the host names it.
  detail?: ReactNode
  /// Extra status right of the presets, left of power.
  status?: ReactNode
}

/// Title, factory presets and power — the same bar on every built-in.
export function Header({
  name,
  subtitle,
  mark,
  connected,
  presets,
  presetIndex,
  onPreset,
  power,
  onPower,
  detail,
  status,
}: HeaderProps) {
  const [open, setOpen] = useState(false)
  const anchor = useRef<HTMLButtonElement | null>(null)
  const load = (index: number) => onPreset(((index % presets.length) + presets.length) % presets.length)
  const step = (delta: -1 | 1) => load((presetIndex ?? (delta > 0 ? -1 : 0)) + delta)
  const label = presetIndex === null ? 'Modified' : (presets[presetIndex]?.name ?? 'Modified')

  return (
    <header className="flex h-11 shrink-0 items-center gap-3 border-b border-line bg-bar px-3">
      <div className="flex min-w-0 flex-1 items-center gap-2.5">
        <span className="h-5 w-5 shrink-0" aria-hidden="true">
          {mark}
        </span>
        <div className="flex min-w-0 items-baseline gap-2">
          <h1 className="text-[13px] font-bold tracking-[-0.01em] uppercase">{name}</h1>
          <span className="truncate text-[11px] text-ink-3">{subtitle}</span>
        </div>
        <span
          className="h-1.5 w-1.5 shrink-0 rounded-full"
          style={{ background: connected ? 'var(--color-accent)' : 'var(--color-ink-4)' }}
          title={connected ? 'Linked to the insert' : 'Preview — no insert bound'}
          aria-label={connected ? 'Linked to the insert' : 'Preview, no insert bound'}
        />
        {detail}
        {IS_BROWSER_PREVIEW && (
          <span
            className="shrink-0 rounded border border-warn/50 px-1.5 py-0.5 text-[10px] font-semibold text-warn"
            title="Running in a browser: the signal and meters come from a simulated preview host, not the DSP"
          >
            Browser preview · simulated signal
          </span>
        )}
      </div>

      <div className="flex items-center gap-1">
        <IconButton label="Previous preset" onClick={() => step(-1)}>
          <CaretLeftIcon size={13} weight="bold" />
        </IconButton>
        <button
          ref={anchor}
          type="button"
          aria-haspopup="dialog"
          aria-expanded={open}
          onClick={() => setOpen((value) => !value)}
          className={`flex h-7 w-52 cursor-pointer items-center gap-2 rounded-md border px-3 transition-colors duration-150 ${
            open ? 'border-line-hi bg-raised' : 'border-line bg-canvas hover:border-line-hi'
          }`}
        >
          <span className="min-w-0 flex-1 truncate text-left text-[12px] font-medium">{label}</span>
          <CaretDownIcon size={11} weight="bold" className="shrink-0 text-ink-3" />
        </button>
        <AnimatePresence>
          {open && (
            <PresetMenu
              anchorRef={anchor}
              presets={presets}
              currentIndex={presetIndex}
              onLoad={load}
              onClose={() => setOpen(false)}
            />
          )}
        </AnimatePresence>
        <IconButton label="Next preset" onClick={() => step(1)}>
          <CaretRightIcon size={13} weight="bold" />
        </IconButton>
        <IconButton label="Reset to the default preset" onClick={() => load(0)}>
          <ArrowCounterClockwiseIcon size={13} weight="bold" />
        </IconButton>
      </div>

      <div className="flex flex-1 items-center justify-end gap-3">
        {status}
        <PowerButton name={name} on={power} onToggle={() => onPower(!power)} />
      </div>
    </header>
  )
}

/// Factory preset browser: search, arrow keys, Enter. Read-only content.
function PresetMenu({
  anchorRef,
  presets,
  currentIndex,
  onLoad,
  onClose,
}: {
  anchorRef: React.RefObject<HTMLElement | null>
  presets: readonly { name: string }[]
  currentIndex: number | null
  onLoad: (index: number) => void
  onClose: () => void
}) {
  const [query, setQuery] = useState('')
  const [cursor, setCursor] = useState(Math.max(0, currentIndex ?? 0))
  const searchRef = useRef<HTMLInputElement | null>(null)

  useEffect(() => {
    searchRef.current?.focus()
  }, [])

  const matches = useMemo(() => {
    const needle = query.trim().toLowerCase()
    return presets
      .map((preset, index) => ({ preset, index }))
      .filter(({ preset }) => !needle || preset.name.toLowerCase().includes(needle))
  }, [presets, query])

  const active = Math.min(cursor, Math.max(0, matches.length - 1))

  const onKeyDown = (event: React.KeyboardEvent<HTMLInputElement>) => {
    if (event.key === 'ArrowDown') {
      event.preventDefault()
      setCursor(Math.min(active + 1, matches.length - 1))
    } else if (event.key === 'ArrowUp') {
      event.preventDefault()
      setCursor(Math.max(active - 1, 0))
    } else if (event.key === 'Enter' && matches[active]) {
      event.preventDefault()
      onLoad(matches[active].index)
      onClose()
    }
  }

  return (
    <Popover anchorRef={anchorRef} onClose={onClose} align="center" width={260}>
      <div className="flex items-center gap-2 border-b border-line px-3 py-2">
        <MagnifyingGlassIcon size={13} className="shrink-0 text-ink-3" />
        <input
          ref={searchRef}
          type="search"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          onKeyDown={onKeyDown}
          placeholder="Search presets"
          aria-label="Search factory presets"
          className="min-w-0 flex-1 bg-transparent text-[12px] text-ink outline-none placeholder:text-ink-3"
        />
      </div>
      <div role="listbox" aria-label="Factory presets" className="max-h-72 overflow-y-auto py-1">
        {matches.length === 0 ? (
          <p className="px-3 py-5 text-center text-[11px] text-ink-3">No preset matches “{query}”.</p>
        ) : (
          matches.map(({ preset, index }, position) => (
            <button
              key={preset.name}
              type="button"
              role="option"
              aria-selected={index === currentIndex}
              onMouseEnter={() => setCursor(position)}
              onClick={() => {
                onLoad(index)
                onClose()
              }}
              className="flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-left text-[12px] transition-colors duration-150"
              style={{ background: position === active ? 'rgb(255 255 255 / 0.05)' : undefined }}
            >
              <span className="grid w-4 shrink-0 place-items-center">
                {index === currentIndex && <CheckIcon size={12} weight="bold" className="text-accent" />}
              </span>
              <span className="min-w-0 flex-1 truncate">{preset.name}</span>
            </button>
          ))
        )}
      </div>
    </Popover>
  )
}
