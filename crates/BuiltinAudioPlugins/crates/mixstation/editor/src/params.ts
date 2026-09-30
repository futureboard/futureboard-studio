import {
  NUMERIC_KEYS,
  defaults,
  type MixStationParams,
} from './bridge'

export type NumericParamId = (typeof NUMERIC_KEYS)[number]

export type ParamSpec = {
  id: NumericParamId
  label: string
  min: number
  max: number
  step: number
  unit: string
  defaultValue: number
  taper?: 'linear' | 'log'
}

function spec(
  id: NumericParamId,
  label: string,
  min: number,
  max: number,
  step: number,
  unit: string,
  taper?: ParamSpec['taper'],
): ParamSpec {
  return { id, label, min, max, step, unit, defaultValue: defaults[id], taper }
}

export const PARAM_SPECS = {
  inputTrimDb: spec('inputTrimDb', 'Input', -24, 24, 0.1, 'dB'),
  hpfHz: spec('hpfHz', 'Low Cut', 20, 500, 1, 'Hz', 'log'),
  lpfHz: spec('lpfHz', 'High Cut', 1_000, 20_000, 10, 'Hz', 'log'),
  lowGainDb: spec('lowGainDb', 'Low', -18, 18, 0.1, 'dB'),
  lowMidFreqHz: spec('lowMidFreqHz', 'LM Freq', 80, 2_000, 1, 'Hz', 'log'),
  lowMidGainDb: spec('lowMidGainDb', 'LM Gain', -18, 18, 0.1, 'dB'),
  highMidFreqHz: spec('highMidFreqHz', 'HM Freq', 500, 12_000, 1, 'Hz', 'log'),
  highMidGainDb: spec('highMidGainDb', 'HM Gain', -18, 18, 0.1, 'dB'),
  highGainDb: spec('highGainDb', 'High', -18, 18, 0.1, 'dB'),
  compThresholdDb: spec('compThresholdDb', 'Threshold', -60, 0, 0.1, 'dB'),
  compRatio: spec('compRatio', 'Ratio', 1, 20, 0.1, ':1'),
  compAttackMs: spec('compAttackMs', 'Attack', 0.1, 100, 0.1, 'ms', 'log'),
  compReleaseMs: spec('compReleaseMs', 'Release', 10, 1_000, 1, 'ms', 'log'),
  compMakeupDb: spec('compMakeupDb', 'Makeup', -12, 24, 0.1, 'dB'),
  satDrivePct: spec('satDrivePct', 'Drive', 0, 100, 1, '%'),
  satCharacterPct: spec('satCharacterPct', 'Character', 0, 100, 1, '%'),
  widthPct: spec('widthPct', 'Width', 0, 200, 1, '%'),
  outputTrimDb: spec('outputTrimDb', 'Output', -24, 24, 0.1, 'dB'),
  limiterCeilingDb: spec('limiterCeilingDb', 'Ceiling', -12, 0, 0.1, 'dB'),
  limiterReleaseMs: spec('limiterReleaseMs', 'Release', 10, 1_000, 1, 'ms', 'log'),
  slot1Module: spec('slot1Module', 'Slot 1', 0, 6, 1, ''),
  slot2Module: spec('slot2Module', 'Slot 2', 0, 6, 1, ''),
  slot3Module: spec('slot3Module', 'Slot 3', 0, 6, 1, ''),
  slot4Module: spec('slot4Module', 'Slot 4', 0, 6, 1, ''),
  slot5Module: spec('slot5Module', 'Slot 5', 0, 6, 1, ''),
  slot6Module: spec('slot6Module', 'Slot 6', 0, 6, 1, ''),
  filtersTrimDb: spec('filtersTrimDb', 'Trim', -24, 24, 0.1, 'dB'),
  eqTrimDb: spec('eqTrimDb', 'Trim', -24, 24, 0.1, 'dB'),
  compTrimDb: spec('compTrimDb', 'Trim', -24, 24, 0.1, 'dB'),
  satTrimDb: spec('satTrimDb', 'Trim', -24, 24, 0.1, 'dB'),
  widthTrimDb: spec('widthTrimDb', 'Trim', -24, 24, 0.1, 'dB'),
  limiterTrimDb: spec('limiterTrimDb', 'Trim', -24, 24, 0.1, 'dB'),
} as const satisfies Record<NumericParamId, ParamSpec>

export function clamp(value: number, min: number, max: number) {
  return Math.min(max, Math.max(min, value))
}

export function sanitizeParams(params: MixStationParams): MixStationParams {
  const next = { ...params }
  for (const id of NUMERIC_KEYS) {
    const item = PARAM_SPECS[id]
    next[id] = clamp(params[id], item.min, item.max)
  }
  return next
}

export function normalizedValue(specification: ParamSpec, value: number) {
  const valueInRange = clamp(value, specification.min, specification.max)
  if (specification.taper === 'log') {
    return (
      Math.log(valueInRange / specification.min) /
      Math.log(specification.max / specification.min)
    )
  }
  return (valueInRange - specification.min) / (specification.max - specification.min)
}

export function valueFromNormalized(specification: ParamSpec, normalized: number) {
  const amount = clamp(normalized, 0, 1)
  const raw =
    specification.taper === 'log'
      ? specification.min *
        Math.pow(specification.max / specification.min, amount)
      : specification.min + amount * (specification.max - specification.min)
  return clamp(
    Math.round(raw / specification.step) * specification.step,
    specification.min,
    specification.max,
  )
}

