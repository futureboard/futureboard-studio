/**
 * Factory presets for Imager.
 *
 * Editor-side starting points that push real parameter values through the
 * existing bridge. Rust still clamps and owns the DSP state — a preset never
 * invents a control or bypasses the wire contract. Every preset leaves solo
 * off and power on: a preset is a sound, not an audition.
 */

import { postParam } from '../bridge'
import {
  BAND_COUNT,
  CROSSOVER_COUNT,
  DEFAULT_PARAMS,
  SOLO_NONE,
  cloneParams,
  crossoverId,
  paramsMatch,
  widthId,
  type ImagerParams,
} from './params'

export type FactoryPreset = {
  name: string
  params: ImagerParams
}

function preset(
  name: string,
  width: [number, number, number, number],
  crossoverHz: [number, number, number] = [120, 1_000, 6_000],
  outputDb = 0,
): FactoryPreset {
  return {
    name,
    params: { power: true, soloBand: SOLO_NONE, crossoverHz, width, outputDb },
  }
}

export const FACTORY_PRESETS: FactoryPreset[] = [
  { name: 'Default', params: cloneParams(DEFAULT_PARAMS) },
  // Low end folded to mono so the kick and bass sit in the centre; the rest
  // untouched.
  preset('Mono Bass', [0, 100, 100, 100], [150, 1_000, 6_000]),
  preset('Master Polish', [40, 100, 115, 130], [120, 800, 7_000]),
  preset('Wide Air', [100, 100, 120, 160], [120, 1_500, 9_000]),
  preset('Vocal Focus', [70, 60, 100, 120], [200, 1_200, 5_000]),
  preset('Drum Bus', [30, 90, 120, 135], [110, 700, 5_500]),
  preset('Super Wide', [60, 140, 170, 200], [150, 1_200, 6_000], -1.5),
  preset('Narrow', [50, 50, 50, 50]),
  // Every band folded — a mono compatibility check, not a mix setting.
  preset('Mono Check', [0, 0, 0, 0]),
]

export function matchingPresetIndex(params: ImagerParams): number | null {
  const index = FACTORY_PRESETS.findIndex((entry) => paramsMatch(params, entry.params))
  return index >= 0 ? index : null
}

/// Whole-state push; the bridge coalesces it into one `setParams` per frame.
export function postAllParams(params: ImagerParams) {
  postParam('power', params.power ? 1 : 0)
  for (let i = 0; i < CROSSOVER_COUNT; i++) postParam(crossoverId(i), params.crossoverHz[i]!)
  for (let i = 0; i < BAND_COUNT; i++) postParam(widthId(i), params.width[i]!)
  postParam('soloBand', params.soloBand)
  postParam('outputDb', params.outputDb)
}
