import { Knob } from './Knob'
import { formatDb } from './math'
import { normalizedValue, type ParamSpec } from './params'

/// A readout for one of MixStation's parameters, in its own unit's language.
export function formatParam(spec: ParamSpec, value: number): string {
  switch (spec.unit) {
    case 'Hz':
      return value >= 1000 ? `${(value / 1000).toFixed(value >= 10_000 ? 1 : 2)}k` : `${Math.round(value)}`
    case 'dB':
      return formatDb(value)
    case 'ms':
      return value < 10 ? value.toFixed(1) : `${Math.round(value)}`
    case ':1':
      return value >= 10 ? value.toFixed(0) : value.toFixed(1)
    default:
      return `${Math.round(value)}`
  }
}

/// The shared knob driven by a MixStation `ParamSpec`: range, step, default
/// and log taper all come from the spec, so the knob holds no ranges of its own.
export function ParamKnob({
  spec,
  value,
  onChange,
  size = 44,
  disabled = false,
  bipolar = false,
  disabledHint,
}: {
  spec: ParamSpec
  value: number
  onChange: (value: number) => void
  size?: number
  disabled?: boolean
  /// Fill from the default — for trims and gains whose neutral point is 0.
  bipolar?: boolean
  disabledHint?: string
}) {
  const log = spec.taper === 'log'
  return (
    <Knob
      label={spec.label}
      value={value}
      min={spec.min}
      max={spec.max}
      step={spec.step}
      unit={spec.unit === ':1' ? ': 1' : spec.unit || undefined}
      format={(v) => formatParam(spec, v)}
      defaultValue={spec.defaultValue}
      originAtDefault={bipolar}
      toProgress={log ? (v) => normalizedValue(spec, v) : undefined}
      fromProgress={log ? (p) => spec.min * Math.pow(spec.max / spec.min, p) : undefined}
      size={size}
      disabled={disabled}
      disabledHint={disabledHint}
      onChange={onChange}
    />
  )
}
