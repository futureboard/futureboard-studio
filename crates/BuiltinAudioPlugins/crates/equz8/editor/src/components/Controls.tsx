import type { ReactNode } from 'react'
import { PowerIcon } from '@phosphor-icons/react'

/// Ghost icon button, 28 px. `active` latches it: tinted fill and brighter
/// glyph, so the state reads on two channels rather than colour alone.
export function IconButton({
  label,
  onClick,
  children,
  active = false,
  disabled = false,
}: {
  label: string
  onClick?: () => void
  children: ReactNode
  active?: boolean
  disabled?: boolean
}) {
  return (
    <button
      type="button"
      aria-label={label}
      aria-pressed={active}
      title={label}
      disabled={disabled}
      onClick={onClick}
      className={`grid h-7 w-7 shrink-0 cursor-pointer place-items-center rounded-md transition-colors duration-150 disabled:cursor-not-allowed disabled:opacity-30 ${
        active
          ? 'bg-accent/15 text-accent-hi hover:bg-accent/25'
          : 'text-ink-3 hover:bg-white/6 hover:text-ink'
      }`}
    >
      {children}
    </button>
  )
}

/// The plug-in's own power. On is the accent; off is a hollow, muted glyph.
export function PowerButton({ on, onToggle }: { on: boolean; onToggle: () => void }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={on ? 'EQ on — click to bypass' : 'EQ bypassed — click to turn on'}
      title={on ? 'Bypass the EQ' : 'Turn the EQ on'}
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

/// A small latched pill (Solo, Dynamic).
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
