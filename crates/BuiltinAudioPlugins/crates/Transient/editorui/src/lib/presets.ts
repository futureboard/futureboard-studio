/**
 * Factory presets for Transient: editor-side starting points pushed through
 * the real parameter bridge. Rust still clamps and owns the DSP state.
 */

import { DEFAULT_PARAMS, paramsMatch, type TransientParams } from './params'

export type FactoryPreset = { name: string; params: TransientParams }

const preset = (name: string, patch: Partial<TransientParams>): FactoryPreset => ({
  name,
  params: { ...DEFAULT_PARAMS, ...patch, power: true },
})

export const FACTORY_PRESETS: readonly FactoryPreset[] = [
  preset('Default', {}),
  preset('Punch Up', { attack: 55, sustain: -15, speed: 60 }),
  preset('Snap Cut', { attack: -45, sustain: 20, speed: 70 }),
  preset('Body Boost', { attack: 10, sustain: 50, speed: 40 }),
  preset('Drum Gate', { attack: 35, sustain: -70, speed: 80 }),
]

export function matchingPresetIndex(params: TransientParams) {
  const index = FACTORY_PRESETS.findIndex((entry) => paramsMatch(params, entry.params))
  return index >= 0 ? index : null
}
