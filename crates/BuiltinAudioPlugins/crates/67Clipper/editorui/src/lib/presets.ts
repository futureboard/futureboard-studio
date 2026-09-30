/**
 * Factory presets for 67Clipper: editor-side starting points pushed through
 * the real parameter bridge. Rust still clamps and owns the DSP state.
 */

import { DEFAULT_PARAMS, paramsMatch, type Clipper67Params } from './params'

export type FactoryPreset = { name: string; params: Clipper67Params }

const preset = (name: string, patch: Partial<Clipper67Params>): FactoryPreset => ({
  name,
  params: { ...DEFAULT_PARAMS, ...patch, power: true },
})

export const FACTORY_PRESETS: readonly FactoryPreset[] = [
  preset('Default', {}),
  preset('Soft Clip', { mode: 'clip', thresholdDb: -3, shape: 80, ceilingDb: -0.3 }),
  preset('Aggressive', { mode: 'clip', thresholdDb: -12, shape: 12, ceilingDb: -0.1 }),
  preset('Hybrid Glue', { mode: 'hybrid', thresholdDb: -8, shape: 60, ceilingDb: -0.3 }),
  preset('Brick Limit', { mode: 'limit', thresholdDb: -1, shape: 0, ceilingDb: -0.1, dcFilter: false }),
]

export function matchingPresetIndex(params: Clipper67Params) {
  const index = FACTORY_PRESETS.findIndex((entry) => paramsMatch(params, entry.params))
  return index >= 0 ? index : null
}
