import { useEffect, useMemo, useRef, useState } from 'react'
import { CheckIcon, MagnifyingGlassIcon } from '@phosphor-icons/react'
import { FACTORY_PRESETS } from '../lib/presets'
import { Popover } from './Popover'

/**
 * Factory preset browser — the same pattern EQUZ8 and MixStation use.
 *
 * Presets are read-only factory content; no save/rename/delete UI.
 */
export function PresetMenu({
  anchorRef,
  currentIndex,
  onLoad,
  onClose,
}: {
  anchorRef: React.RefObject<HTMLElement | null>
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
    return FACTORY_PRESETS.map((preset, index) => ({ preset, index })).filter(
      ({ preset }) => !needle || preset.name.toLowerCase().includes(needle),
    )
  }, [query])

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
    <Popover anchorRef={anchorRef} onClose={onClose} align="center" width={280}>
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
          matches.map(({ preset, index }, position) => {
            const current = index === currentIndex
            return (
              <button
                key={preset.name}
                type="button"
                role="option"
                aria-selected={current}
                onMouseEnter={() => setCursor(position)}
                onClick={() => {
                  onLoad(index)
                  onClose()
                }}
                className="flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-left text-[12px] transition-colors duration-150"
                style={{ background: position === active ? 'rgb(255 255 255 / 0.05)' : undefined }}
              >
                <span className="grid w-4 shrink-0 place-items-center">
                  {current && <CheckIcon size={12} weight="bold" className="text-accent" />}
                </span>
                <span className="min-w-0 flex-1 truncate">{preset.name}</span>
              </button>
            )
          })
        )}
      </div>
    </Popover>
  )
}
