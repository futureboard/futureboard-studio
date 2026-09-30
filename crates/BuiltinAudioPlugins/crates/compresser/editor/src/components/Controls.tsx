import type { ReactNode } from 'react'
import { PowerIcon } from '@phosphor-icons/react'

/// The plug-in's own power. On is the accent; off is a hollow, muted glyph.
export function PowerButton({ on, onToggle }: { on: boolean; onToggle: () => void }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={on ? 'Compressor on — click to bypass' : 'Compressor bypassed — click to turn on'}
      title={on ? 'Bypass the Compressor' : 'Turn the Compressor on'}
      onClick={onToggle}
      className={`grid h-7 w-7 shrink-0 cursor-pointer place-items-center rounded-md border transition-colors duration-150 ${
        on
          ? 'border-accent/60 bg-accent/15 text-accent-hi hover:bg-accent/25'
          : 'border-line-hi text-ink-4 hover:text-ink-2'
      }`}
    >
      <PowerIcon size={14} weight="bold" />
    </button>
  )
}

/// A small latched pill (Solo, Bypass). Latched reads on two channels: tinted
/// fill and border, and the glyph at full strength.
export function Pill({
  label,
  on,
  accent,
  title,
  disabled = false,
  onToggle,
  children,
}: {
  label: string
  on: boolean
  accent: string
  title: string
  disabled?: boolean
  onToggle: () => void
  children?: ReactNode
}) {
  return (
    <button
      type="button"
      aria-pressed={on}
      aria-label={label}
      title={title}
      disabled={disabled}
      onClick={onToggle}
      className="flex h-6 shrink-0 cursor-pointer items-center gap-1 rounded-md border px-2 text-[10.5px] font-semibold transition-colors duration-150 disabled:cursor-not-allowed disabled:opacity-35"
      style={{
        borderColor: on ? `color-mix(in srgb, ${accent} 60%, transparent)` : 'var(--color-line-hi)',
        background: on ? `color-mix(in srgb, ${accent} 18%, transparent)` : 'transparent',
        color: on ? accent : 'var(--color-ink-3)',
      }}
    >
      {children}
      {label}
    </button>
  )
}

/// A two-or-more-way segmented switch. Square inner edges, rounded outer ones;
/// the selected segment is filled.
export function Segmented<T extends string>({
  label,
  value,
  options,
  onChange,
}: {
  label: string
  value: T
  options: readonly { value: T; label: string; title: string }[]
  onChange: (value: T) => void
}) {
  return (
    <div role="radiogroup" aria-label={label} className="flex h-7 rounded-md border border-line bg-canvas p-0.5">
      {options.map((option) => {
        const selected = option.value === value
        return (
          <button
            key={option.value}
            type="button"
            role="radio"
            aria-checked={selected}
            title={option.title}
            onClick={() => onChange(option.value)}
            className={`cursor-pointer rounded-[4px] px-3 text-[11.5px] font-semibold transition-colors duration-150 ${
              selected ? 'bg-raised text-ink shadow-[inset_0_0_0_1px_var(--color-line-hi)]' : 'text-ink-3 hover:text-ink'
            }`}
          >
            {option.label}
          </button>
        )
      })}
    </div>
  )
}
