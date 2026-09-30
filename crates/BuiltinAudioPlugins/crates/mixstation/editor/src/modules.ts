import type { MixStationParams } from './bridge'
import type { NumericParamId } from './params'

/**
 * The rack catalogue.
 *
 * `code` is the slot value the Rust side stores in `slotNModule` and matches
 * the arms of `Dsp::process_stereo`; it is the stable identifier, not the array
 * position. `enabledId` is the module's own bypass parameter. Adding a module
 * here without a matching arm in Rust would produce a control that does
 * nothing, so the two must move together.
 */
export type RackModuleId = Exclude<BooleanParamId, 'power'>

export type BooleanParamId =
  | 'power'
  | 'filtersEnabled'
  | 'eqEnabled'
  | 'compEnabled'
  | 'satEnabled'
  | 'widthEnabled'
  | 'limiterEnabled'

export type RackModule = {
  code: number
  enabledId: RackModuleId
  name: string
  hint: string
  /** Knob row, in panel order. */
  knobs: readonly NumericParamId[]
  /** This module's own output trim, applied after its stage in the Rust DSP. */
  trimId: NumericParamId
}

export const RACK_MODULES: readonly RackModule[] = [
  {
    code: 1,
    enabledId: 'filtersEnabled',
    name: 'Filters',
    hint: '24 dB/oct high and low cut',
    knobs: ['hpfHz', 'lpfHz'],
    trimId: 'filtersTrimDb',
  },
  {
    code: 2,
    enabledId: 'eqEnabled',
    name: 'EQ',
    hint: 'Four-band with proportional-Q mids',
    knobs: ['lowGainDb', 'lowMidFreqHz', 'lowMidGainDb', 'highMidFreqHz', 'highMidGainDb', 'highGainDb'],
    trimId: 'eqTrimDb',
  },
  {
    code: 3,
    enabledId: 'compEnabled',
    name: 'Compressor',
    hint: 'Stereo-linked, program-dependent release',
    knobs: [
      'compThresholdDb',
      'compRatio',
      'compAttackMs',
      'compReleaseMs',
      'compMakeupDb',
    ],
    trimId: 'compTrimDb',
  },
  {
    code: 4,
    enabledId: 'satEnabled',
    name: 'Drive',
    hint: 'Anti-aliased asymmetric saturation',
    knobs: ['satDrivePct', 'satCharacterPct'],
    trimId: 'satTrimDb',
  },
  {
    code: 5,
    enabledId: 'widthEnabled',
    name: 'Width',
    hint: 'Mid/side stereo image',
    knobs: ['widthPct'],
    trimId: 'widthTrimDb',
  },
  {
    code: 6,
    enabledId: 'limiterEnabled',
    name: 'Limiter',
    hint: 'Zero-latency brickwall ceiling',
    knobs: ['limiterCeilingDb', 'limiterReleaseMs'],
    trimId: 'limiterTrimDb',
  },
] as const

export const SLOT_IDS = [
  'slot1Module',
  'slot2Module',
  'slot3Module',
  'slot4Module',
  'slot5Module',
  'slot6Module',
] as const satisfies readonly NumericParamId[]

export const BOOLEAN_LABELS: Record<RackModuleId, string> = {
  filtersEnabled: 'Filters',
  eqEnabled: 'EQ',
  compEnabled: 'Compressor',
  satEnabled: 'Drive',
  widthEnabled: 'Width',
  limiterEnabled: 'Limiter',
}

export function moduleByCode(code: number) {
  return RACK_MODULES.find((item) => item.code === code)
}

/** Slot codes in chain order, as integers. */
export function slotCodes(params: MixStationParams) {
  return SLOT_IDS.map((id) => Math.round(params[id]))
}

