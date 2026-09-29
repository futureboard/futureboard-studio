import type { ReactNode } from 'react'

/// A latched pill (Reverse, Mute, Solo). The latched state reads on two
/// channels — fill and label weight — never colour alone.
export function Toggle({
  label,
  on,
  title,
  tone = 'accent',
  onToggle,
}: {
  label: string
  on: boolean
  title: string
  tone?: 'accent' | 'warn'
  onToggle: () => void
}) {
  return (
    <button
      type="button"
      className={`toggle tone-${tone} ${on ? 'is-on' : ''}`}
      aria-pressed={on}
      title={title}
      onClick={onToggle}
    >
      {label}
    </button>
  )
}

/// A segmented choice (filter mode).
export function Segmented<T extends string>({
  label,
  value,
  options,
  onChange,
}: {
  label: string
  value: T
  options: { value: T; label: string }[]
  onChange: (value: T) => void
}) {
  return (
    <div className="segmented" role="radiogroup" aria-label={label}>
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          role="radio"
          aria-checked={option.value === value}
          className={option.value === value ? 'is-on' : ''}
          onClick={() => onChange(option.value)}
        >
          {option.label}
        </button>
      ))}
    </div>
  )
}

/// − value + with keyboard arrows, for small integer ranges (note, choke).
export function Stepper({
  label,
  value,
  min,
  max,
  display,
  onChange,
}: {
  label: string
  value: number
  min: number
  max: number
  display: (value: number) => ReactNode
  onChange: (value: number) => void
}) {
  const set = (next: number) => onChange(Math.min(max, Math.max(min, next)))
  return (
    <div className="stepper">
      <span className="cap">{label}</span>
      <div
        className="stepper-body"
        role="spinbutton"
        tabIndex={0}
        aria-label={label}
        aria-valuemin={min}
        aria-valuemax={max}
        aria-valuenow={value}
        onKeyDown={(event) => {
          if (event.key === 'ArrowUp' || event.key === 'ArrowRight') {
            event.preventDefault()
            set(value + 1)
          } else if (event.key === 'ArrowDown' || event.key === 'ArrowLeft') {
            event.preventDefault()
            set(value - 1)
          }
        }}
      >
        <button type="button" aria-label={`Lower ${label}`} disabled={value <= min} onClick={() => set(value - 1)}>
          −
        </button>
        <output className="num">{display(value)}</output>
        <button type="button" aria-label={`Raise ${label}`} disabled={value >= max} onClick={() => set(value + 1)}>
          +
        </button>
      </div>
    </div>
  )
}

/// A module card: a small caption over its controls.
export function Module({
  title,
  aside,
  children,
}: {
  title: string
  aside?: ReactNode
  children: ReactNode
}) {
  return (
    <section className="module">
      <header>
        <h2 className="cap">{title}</h2>
        {aside}
      </header>
      <div className="module-body">{children}</div>
    </section>
  )
}
