/**
 * Dev-only, rough model of Transient for the browser preview: a fresh hit is
 * scaled by Attack, the body by Sustain, each at the plugin's ±18 dB depth,
 * and the meters report the size of the change. It exists to make the
 * displays move while the UI is debugged in a browser — it is not the DSP and
 * is never bundled into the embedded editor.
 */

import { DEFAULT_PARAMS, MAX_SHAPE_DB, wireValues } from '../lib/params'
import { dbToLinear, type InputFrame } from './signal'

export const PREVIEW_PLUGIN_ID = 'transient'
export const PREVIEW_DEFAULTS: Record<string, number> = Object.fromEntries(wireValues(DEFAULT_PARAMS))

export function createPreviewProcessor() {
  return (wire: Record<string, number>, input: InputFrame, _dt: number) => {
    const attack = (wire.attack ?? 0) / 100
    const sustain = (wire.sustain ?? 0) / 100
    const mix = (wire.mix ?? 100) / 100
    const hit = Math.min(1, input.onset)
    const changeDb = MAX_SHAPE_DB * (attack * hit + sustain * (1 - hit) * 0.6)
    const wet = input.peak * dbToLinear(changeDb)
    const peak = input.peak * (1 - mix) + wet * mix
    return {
      peak,
      rms: input.rms * (input.peak > 0 ? peak / input.peak : 1),
      reductionDb: Math.abs(changeDb) * mix,
    }
  }
}
