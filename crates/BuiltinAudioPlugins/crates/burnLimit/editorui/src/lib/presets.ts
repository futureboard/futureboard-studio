/**
 * Factory presets for BurnLimit: editor-side starting points pushed through
 * the real parameter bridge. Rust still clamps and owns the DSP state.
 */

import { DEFAULT_PARAMS, paramsMatch, type BurnLimitParams } from './params'

export type FactoryPreset = { name: string; params: BurnLimitParams }

const preset = (name: string, patch: Partial<BurnLimitParams>): FactoryPreset => ({
  name,
  params: { ...DEFAULT_PARAMS, ...patch, power: true },
})

export const FACTORY_PRESETS: readonly FactoryPreset[] = [
  preset('Default', {}),
  preset('Master Soft', {
    style: 'clean',
    gainDb: 3,
    ceilingDb: -0.5,
    releaseMs: 350,
    lookaheadMs: 4,
  }),
  preset('Loud Punch', {
    style: 'punch',
    gainDb: 8,
    ceilingDb: -0.3,
    releaseMs: 80,
    lookaheadMs: 1.5,
  }),
  preset('Broadcast Safe', {
    style: 'modern',
    gainDb: 4,
    ceilingDb: -1,
    releaseMs: 180,
    lookaheadMs: 5,
  }),
  preset('Clip Heat', {
    style: 'clip',
    gainDb: 12,
    ceilingDb: -0.1,
    releaseMs: 40,
    lookaheadMs: 0.5,
    truePeak: false,
  }),
  preset('Parallel Glue', {
    style: 'punch',
    gainDb: 10,
    ceilingDb: -0.5,
    releaseMs: 120,
    lookaheadMs: 2,
    mix: 45,
  }),
]

export function matchingPresetIndex(params: BurnLimitParams) {
  const index = FACTORY_PRESETS.findIndex((entry) => paramsMatch(params, entry.params))
  return index >= 0 ? index : null
}
