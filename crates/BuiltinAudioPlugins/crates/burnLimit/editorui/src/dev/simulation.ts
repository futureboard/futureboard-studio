/**
 * Dev-only, rough model of BurnLimit for the browser preview: input gain,
 * an instant-attack limiter at the ceiling with the release knob's recovery,
 * and the dry/wet mix. It exists to make the displays move while the UI is
 * debugged in a browser — it is not the DSP and is never bundled into the
 * embedded editor.
 */

import { DEFAULT_PARAMS, wireValues } from '../lib/params'
import { dbToLinear, linearToDb, type InputFrame } from './signal'

export const PREVIEW_PLUGIN_ID = 'burnlimit'
export const PREVIEW_DEFAULTS: Record<string, number> = Object.fromEntries(wireValues(DEFAULT_PARAMS))

export function createPreviewProcessor() {
  let reductionDb = 0
  return (wire: Record<string, number>, input: InputFrame, dt: number) => {
    const gain = dbToLinear(wire.gainDb ?? 0)
    const ceiling = wire.ceilingDb ?? -0.3
    const releaseSec = Math.max((wire.releaseMs ?? 200) / 1000, 0.005)
    const mix = (wire.mix ?? 100) / 100
    const driven = input.peak * gain
    const needed = Math.max(0, linearToDb(driven) - ceiling)
    const recovery = Math.exp(-dt / releaseSec)
    reductionDb = Math.max(needed, reductionDb * recovery)
    const wet = driven * dbToLinear(-reductionDb)
    const peak = input.peak * (1 - mix) + wet * mix
    const rmsRatio = input.peak > 0 ? peak / input.peak : 1
    return { peak, rms: input.rms * rmsRatio, reductionDb: reductionDb * mix }
  }
}
