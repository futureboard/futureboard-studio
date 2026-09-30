import type { ReactNode } from 'react'
import { PowerIcon } from '@phosphor-icons/react'

/// Ghost icon button, 28 px.
export function IconButton({
  label,
  onClick,
  children,
  disabled = false,
}: {
  label: string
  onClick?: () => void
  children: ReactNode
  disabled?: boolean
}) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      disabled={disabled}
      onClick={onClick}
      className="grid h-7 w-7 shrink-0 cursor-pointer place-items-center rounded-md text-ink-3 transition-colors duration-150 hover:bg-white/6 hover:text-ink disabled:cursor-not-allowed disabled:opacity-30"
    >
      {children}
    </button>
  )
}

/// The plug-in's own power. On is the accent; off is a hollow, muted glyph.
export function PowerButton({ name, on, onToggle }: { name: string; on: boolean; onToggle: () => void }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={on ? `${name} on — click to bypass` : `${name} bypassed — click to turn on`}
      title={on ? `Bypass ${name}` : `Turn ${name} on`}
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

/// A latched switch. Latched reads on two channels: tinted fill and border,
/// and the label at full strength.
export function Toggle({
  label,
  on,
  title,
  onToggle,
}: {
  label: string
  on: boolean
  title: string
  onToggle: () => void
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      title={title}
      onClick={onToggle}
      className={`flex h-7 shrink-0 cursor-pointer items-center gap-1.5 rounded-md border px-2.5 text-[11px] font-semibold transition-colors duration-150 ${
        on ? 'border-accent/55 bg-accent/15 text-accent-hi' : 'border-line-hi text-ink-3 hover:text-ink'
      }`}
    >
      <span className={`h-1.5 w-1.5 rounded-full ${on ? 'bg-accent' : 'bg-ink-4'}`} aria-hidden="true" />
      {label}
    </button>
  )
}

/// A segmented switch: square inner edges, rounded outer ones; the selected
/// segment is filled.
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

/// A titled group inside the control strip.
export function Group({ title, children, className = '' }: { title: string; children: ReactNode; className?: string }) {
  return (
    <section className={`flex min-w-0 flex-col gap-2 ${className}`} aria-label={title}>
      <span className="cap">{title}</span>
      <div className="flex min-w-0 items-start gap-3">{children}</div>
    </section>
  )
}

/// Shown over a display while the plug-in is bypassed.
export function BypassNote({ name }: { name: string }) {
  return (
    <div className="pointer-events-none absolute inset-x-0 bottom-6 flex justify-center">
      <span className="rounded-md border border-line bg-bar/90 px-3 py-1.5 text-[11px] text-ink-2">
        Bypassed — {name} passes audio through unchanged
      </span>
    </div>
  )
}
