/**
 * Dev-only, rough model of 67Clipper for the browser preview: the input is
 * driven by −Threshold into a clipper at full scale (softened by Shape, with
 * extra peak riding in Hybrid and Limit), then trimmed by the ceiling. It exists to make the displays
 * move while the UI is debugged in a browser — it is not the DSP and is never
 * bundled into the embedded editor.
 */

import { DEFAULT_PARAMS, wireValues } from '../lib/params'
import { dbToLinear, linearToDb, type InputFrame } from './signal'

export const PREVIEW_PLUGIN_ID = 'clipper67'
export const PREVIEW_DEFAULTS: Record<string, number> = Object.fromEntries(wireValues(DEFAULT_PARAMS))

export function createPreviewProcessor() {
  let riding = 0
  return (wire: Record<string, number>, input: InputFrame, dt: number) => {
    const threshold = wire.thresholdDb ?? -6
    const softness = (wire.shape ?? 50) / 100
    const ceiling = wire.ceilingDb ?? -0.3
    const mix = (wire.mix ?? 100) / 100
    const mode = Math.round(wire.mode ?? 0)
    const driven = input.peak * dbToLinear(-threshold)
    const over = Math.max(0, linearToDb(driven))
    // A soft shape starts bending before full scale and gives a little back.
    const clip = over * (1 - 0.25 * softness)
    riding = mode === 0 ? clip : Math.max(clip, riding * Math.exp(-dt / (mode === 2 ? 0.08 : 0.03)))
    const wet = driven * dbToLinear(-riding) * dbToLinear(ceiling)
    const peak = input.peak * (1 - mix) + wet * mix
    return { peak, rms: input.rms * (input.peak > 0 ? peak / input.peak : 1), reductionDb: riding * mix }
  }
}
