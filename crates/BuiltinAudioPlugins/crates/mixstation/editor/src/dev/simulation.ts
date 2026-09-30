/**
 * Dev-only, rough model of the MixStation rack for the browser preview: the
 * groove's peak level walks the loaded slots in order, each stage nudging it
 * the way its controls suggest, and a synthetic analyser frame is shaped like
 * a drum mix. It exists to make the displays move while the UI is debugged in
 * a browser — it is not the DSP and is never bundled into the embedded editor.
 */

import { dbToLinear, linearToDb, type InputFrame } from './signal'

const SLOTS = 6
const SPECTRUM_BINS = 96
const SPECTRUM_MIN_HZ = 20
const SPECTRUM_MAX_HZ = 20_000
const FLOOR_DB = -96
const CEIL_DB = 0

export function createRackModel() {
  let compDb = 0
  let limitDb = 0
  return (wire: Record<string, number>, input: InputFrame, dt: number) => {
    const on = (id: string) => (wire[id] ?? 0) >= 0.5
    const bypassed = !on('power')
    let level = input.peak * dbToLinear(bypassed ? 0 : (wire.inputTrimDb ?? 0))
    const slotIn = new Array<number>(SLOTS).fill(0)
    const slotOut = new Array<number>(SLOTS).fill(0)
    let reduction = 0

    for (let slot = 0; slot < SLOTS; slot++) {
      const code = Math.round(wire[`slot${slot + 1}Module`] ?? 0)
      if (code === 0) continue
      slotIn[slot] = level
      const trim = (id: string) => dbToLinear(wire[id] ?? 0)
      const run = !bypassed
      switch (code) {
        case 1:
          if (run && on('filtersEnabled')) level *= ((wire.hpfHz ?? 30) > 60 ? 0.85 : 0.97) * trim('filtersTrimDb')
          break
        case 2:
          if (run && on('eqEnabled')) {
            const tilt = ((wire.lowGainDb ?? 0) + (wire.lowMidGainDb ?? 0) + (wire.highMidGainDb ?? 0) + (wire.highGainDb ?? 0)) / 4
            level *= dbToLinear(tilt) * trim('eqTrimDb')
          }
          break
        case 3:
          if (run && on('compEnabled')) {
            const threshold = wire.compThresholdDb ?? -18
            const ratio = Math.max(wire.compRatio ?? 4, 1)
            const target = Math.max(0, linearToDb(level) - threshold) * (1 - 1 / ratio)
            const release = Math.max((wire.compReleaseMs ?? 120) / 1000, 0.01)
            compDb = Math.max(target, compDb * Math.exp(-dt / release))
            reduction += compDb
            level *= dbToLinear(-compDb + (wire.compMakeupDb ?? 0)) * trim('compTrimDb')
          }
          break
        case 4:
          if (run && on('satEnabled')) level = Math.tanh(level * (1 + (wire.satDrivePct ?? 0) / 40)) * trim('satTrimDb')
          break
        case 5:
          if (run && on('widthEnabled')) level *= trim('widthTrimDb')
          break
        case 6:
          if (run && on('limiterEnabled')) {
            const ceiling = wire.limiterCeilingDb ?? -0.3
            const target = Math.max(0, linearToDb(level) - ceiling)
            const release = Math.max((wire.limiterReleaseMs ?? 100) / 1000, 0.01)
            limitDb = Math.max(target, limitDb * Math.exp(-dt / release))
            reduction += limitDb
            level *= dbToLinear(-limitDb) * trim('limiterTrimDb')
          }
          break
      }
      slotOut[slot] = level
    }

    const out = bypassed ? input.peak : level * dbToLinear(wire.outputTrimDb ?? 0)
    const ratio = input.peak > 0 ? out / input.peak : 1
    return {
      inPeak: input.peak,
      inRms: input.rms,
      outPeak: out,
      outRms: input.rms * ratio,
      gainReductionDb: bypassed ? 0 : reduction,
      inClip: input.peak >= 1,
      outClip: out >= 1,
      slotInPeak: slotIn,
      slotOutPeak: slotOut,
    }
  }
}

/// A drum-mix-shaped analyser frame: a pink-ish slope with a kick bump that
/// swells on every hit, a snare body, and air that breathes slowly.
export function spectrumFrame(time: number, onset: number) {
  const bins: number[] = []
  for (let i = 0; i < SPECTRUM_BINS; i++) {
    const hz = SPECTRUM_MIN_HZ * Math.pow(SPECTRUM_MAX_HZ / SPECTRUM_MIN_HZ, i / (SPECTRUM_BINS - 1))
    const octave = Math.log2(hz / 100)
    let db = -34 - 3.2 * octave
    db += 14 * onset * Math.exp(-((Math.log2(hz / 60) / 0.6) ** 2))
    db += 6 * Math.exp(-((Math.log2(hz / 220) / 0.8) ** 2))
    db += 3 * Math.sin(time * 0.7 + i * 0.3) * Math.exp(-((Math.log2(hz / 8000) / 1.2) ** 2))
    db += (Math.sin(i * 12.9898 + time * 7) * 43758.5453) % 1 * 2
    bins.push(Math.round(Math.max(0, Math.min(1, (db - FLOOR_DB) / (CEIL_DB - FLOOR_DB))) * 255))
  }
  return { minHz: SPECTRUM_MIN_HZ, maxHz: SPECTRUM_MAX_HZ, floorDb: FLOOR_DB, ceilDb: CEIL_DB, bins }
}
