import { describe, expect, test } from 'bun:test'

import type { VerbParams } from './bridge'
import {
  BASE_LINE_MS,
  MODE_DIFFUSION_BIAS,
  MODE_LINE_SCALE,
  decayModel,
  diffusionGain,
  levelDbAt,
  lineDelaysMs,
} from './model'
import { MODES, MODE_HINTS, PARAMS } from './params'
import { LIB_RS, rustFloatArray, rustModeArms } from './rust'

const defaults: VerbParams = {
  power: true,
  mode: 'hall',
  predelayMs: PARAMS.predelayMs.default,
  size: PARAMS.size.default,
  decaySec: PARAMS.decaySec.default,
  diffusion: PARAMS.diffusion.default,
  damping: PARAMS.damping.default,
  bassMult: PARAMS.bassMult.default,
  modDepth: PARAMS.modDepth.default,
  modRateHz: PARAMS.modRateHz.default,
  lowCutHz: PARAMS.lowCutHz.default,
  highCutHz: PARAMS.highCutHz.default,
  width: PARAMS.width.default,
  mix: PARAMS.mix.default,
  outputDb: PARAMS.outputDb.default,
  freeze: false,
}

describe('the display model mirrors the Rust tank', () => {
  test('base line lengths match verbspace::BASE_LINE_MS', () => {
    expect(BASE_LINE_MS).toEqual(rustFloatArray(LIB_RS, 'BASE_LINE_MS'))
  })

  test('mode scaling matches ReverbMode::line_scale', () => {
    expect(MODE_LINE_SCALE).toEqual(rustModeArms('line_scale') as never)
  })

  test('mode diffusion bias matches ReverbMode::diffusion_bias', () => {
    expect(MODE_DIFFUSION_BIAS).toEqual(rustModeArms('diffusion_bias') as never)
  })

  test('line lengths use the same size scaling as apply_params_scalars', () => {
    const size = 60
    const scale = MODE_LINE_SCALE.plate * (0.35 + (size / 100) * 0.85)
    const actual = lineDelaysMs('plate', size)
    expect(actual).toHaveLength(BASE_LINE_MS.length)
    for (const [index, ms] of BASE_LINE_MS.entries()) {
      expect(actual[index]).toBeCloseTo(ms * scale, 9)
    }
  })

  test('diffusion gain spans the same bias..0.78 window as Rust', () => {
    for (const mode of MODES) {
      expect(diffusionGain(mode, 0)).toBeCloseTo(MODE_DIFFUSION_BIAS[mode], 6)
      expect(diffusionGain(mode, 100)).toBeCloseTo(0.78, 6)
    }
  })
})

describe('decay bands', () => {
  test('bass multiplier stretches the low band, damping shortens the high one', () => {
    const model = decayModel(defaults)
    expect(model.midSec).toBe(defaults.decaySec)
    expect(model.lowSec).toBeCloseTo(defaults.decaySec * defaults.bassMult, 6)
    expect(model.highSec).toBeLessThan(model.midSec)
  })

  test('zero damping leaves the high band equal to the mid band', () => {
    const model = decayModel({ ...defaults, damping: 0 })
    expect(model.highSec).toBeCloseTo(model.midSec, 6)
  })

  test('more damping is always a shorter high band', () => {
    let previous = Infinity
    for (const damping of [0, 25, 50, 75, 100]) {
      const { highSec } = decayModel({ ...defaults, damping })
      expect(highSec).toBeLessThanOrEqual(previous)
      expect(highSec).toBeGreaterThan(0)
      previous = highSec
    }
  })

  /** Freeze holds the tail, so the display must not draw it decaying. */
  test('freeze reports an unbounded tail rather than a number', () => {
    const model = decayModel({ ...defaults, freeze: true })
    expect(model.midSec).toBe(Infinity)
    expect(model.lowSec).toBe(Infinity)
    expect(levelDbAt(60, 0.02, model.midSec)).toBe(0)
  })

  test('nothing is drawn before the pre-delay', () => {
    expect(levelDbAt(0.01, 0.02, 2.4)).toBe(-Infinity)
  })

  test('the envelope is -60 dB exactly one RT60 after the pre-delay', () => {
    expect(levelDbAt(0.02 + 2.4, 0.02, 2.4)).toBeCloseTo(-60, 6)
  })
})

/**
 * The space picker describes each mode in words. The words are only allowed
 * to say what the constants say, so they are held against them here.
 */
describe('the mode hints describe the tank each mode builds', () => {
  const byScale = [...MODES].sort(
    (a, b) => MODE_LINE_SCALE[a] - MODE_LINE_SCALE[b],
  )
  const byBias = [...MODES].sort(
    (a, b) => MODE_DIFFUSION_BIAS[a] - MODE_DIFFUSION_BIAS[b],
  )

  test('every mode has a hint', () => {
    for (const mode of MODES) expect(MODE_HINTS[mode].length).toBeGreaterThan(0)
  })

  test('largest and smallest follow line_scale', () => {
    expect(byScale.at(-1)).toBe('hall')
    expect(MODE_HINTS.hall).toContain('Largest')
    expect(byScale[0]).toBe('ambience')
    expect(MODE_HINTS.ambience).toContain('Smallest')
  })

  test('densest and sparsest follow diffusion_bias', () => {
    expect(byBias.at(-1)).toBe('plate')
    expect(MODE_HINTS.plate).toContain('densest')
    expect(byBias[0]).toBe('ambience')
    expect(MODE_HINTS.ambience).toContain('sparsest')
  })

  test('room is shorter and less diffuse than chamber, and plate is small', () => {
    expect(MODE_LINE_SCALE.room).toBeLessThan(MODE_LINE_SCALE.chamber)
    expect(MODE_DIFFUSION_BIAS.room).toBeLessThan(MODE_DIFFUSION_BIAS.chamber)
    expect(MODE_LINE_SCALE.plate).toBeLessThan(MODE_LINE_SCALE.room)
  })
})