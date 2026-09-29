/**
 * The editor duplicates the Drum Sampler's wire ids, ranges and envelope
 * math. These tests read the real `.rs` files and compare, so a change on
 * either side that is not mirrored fails here instead of shipping an editor
 * that quietly disagrees with the DSP. Run with `bun test`.
 */

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import {
  DECAY_FLOOR,
  FIELD_SUFFIX,
  FILTER_MODES,
  MIN_REGION,
  PAD_COUNT,
  RANGE,
  defaultPad,
  dropPads,
  effectiveRegion,
  envelopeAt,
  parseKit,
  regionSeconds,
  resonanceQ,
  wireValue,
} from '../src/lib/pads'

const read = (relative: string) => readFileSync(fileURLToPath(new URL(relative, import.meta.url)), 'utf8')
const LIB_RS = read('../../src/lib.rs')
const IPC_RS = read('../../src/ipc.rs')

function rustConst(name: string): number {
  const match = new RegExp(`const ${name}\\s*:\\s*(?:f32|usize)\\s*=\\s*([0-9_.]+)\\s*;`).exec(LIB_RS)
  if (!match?.[1]) throw new Error(`${name} not found in lib.rs`)
  return Number(match[1].replace(/_/g, ''))
}

describe('the editor mirrors drumsampler', () => {
  test('every field suffix is a wire id in ipc.rs', () => {
    for (const suffix of Object.values(FIELD_SUFFIX)) {
      expect(IPC_RS).toContain(`concat!("pad", $n, "${suffix}")`)
    }
    expect(IPC_RS).toContain('"masterGain"')
    expect(IPC_RS).toContain('"masterTune"')
  })

  test('constants match lib.rs', () => {
    expect(PAD_COUNT).toBe(rustConst('PADS'))
    expect(MIN_REGION).toBe(rustConst('MIN_REGION'))
    expect(DECAY_FLOOR).toBe(rustConst('DECAY_FLOOR'))
    expect(RANGE.cutoff[0]).toBe(rustConst('MIN_CUTOFF_HZ'))
    expect(RANGE.cutoff[1]).toBe(rustConst('MAX_CUTOFF_HZ'))
    expect(RANGE.hold[1]).toBe(rustConst('MAX_HOLD_MS'))
    expect(RANGE.decay[1]).toBe(rustConst('MAX_DECAY_MS'))
  })

  test('filter modes are the Rust enum in wire order', () => {
    const body = /pub enum FilterMode \{([\s\S]*?)\n\}/.exec(LIB_RS)?.[1] ?? ''
    const variants = Array.from(body.matchAll(/^\s{4}(\w+),/gm), (m) => m[1]!)
    const camel = variants.map((v) => v[0]!.toLowerCase() + v.slice(1))
    expect([...FILTER_MODES]).toEqual(camel)
    expect(wireValue('filterMode', 'highPass')).toBe(2)
  })

  test('resonance maps to Q the way SvfCoeffs does', () => {
    expect(resonanceQ(0)).toBeCloseTo(Math.SQRT1_2, 6)
    expect(resonanceQ(100)).toBeCloseTo(12, 6)
  })
})

describe('state parsing', () => {
  test('a first-release blob opens at the behaviour it had', () => {
    const legacy = {
      note: 36,
      tuneSemitones: 0,
      gainDb: -2,
      pan: 0,
      chokeGroup: 0,
      attackMs: 1,
      releaseMs: 60,
      reverse: false,
      muted: false,
      solo: false,
      sampleName: 'kick.wav',
    }
    const kit = parseKit({ version: 1, params: { pads: Array(PAD_COUNT).fill(legacy) } })
    expect(kit).not.toBeNull()
    const pad = kit!.pads[0]!
    expect(pad.gain).toBe(-2)
    expect(pad.sampleName).toBe('kick.wav')
    expect([pad.start, pad.end]).toEqual([0, 1])
    expect(pad.filterMode).toBe('off')
    expect(pad.velocity).toBe(100)
    expect(pad.decay).toBe(0)
    expect(kit!.masterGain).toBe(0)
  })

  test('a blob with the wrong pad count is rejected', () => {
    expect(parseKit({ params: { pads: [] } })).toBeNull()
    expect(parseKit(null)).toBeNull()
  })
})

describe('region and envelope', () => {
  test('the region is sorted and never empty, like region_frames', () => {
    const pad = { ...defaultPad(0), start: 0.8, end: 0.2 }
    expect(effectiveRegion(pad)).toEqual([0.2, 0.8])
    const [lo, hi] = effectiveRegion({ ...pad, start: 1, end: 1 })
    expect(hi - lo).toBeCloseTo(MIN_REGION, 9)
  })

  test('region length follows tune and the kit tune', () => {
    const pad = { ...defaultPad(0), start: 0, end: 0.5 }
    expect(regionSeconds(pad, 48_000, 48_000, 0)).toBeCloseTo(0.5, 6)
    expect(regionSeconds({ ...pad, tune: 12 }, 48_000, 48_000, 0)).toBeCloseTo(0.25, 6)
    expect(regionSeconds(pad, 48_000, 48_000, -12)).toBeCloseTo(1, 6)
  })

  test('AHD shape: ramp, hold, then −60 dB at the decay time', () => {
    const pad = { ...defaultPad(0), attack: 10, hold: 20, decay: 100 }
    expect(envelopeAt(pad, 0.005)).toBeCloseTo(0.5, 6)
    expect(envelopeAt(pad, 0.02)).toBe(1)
    expect(envelopeAt(pad, 0.029)).toBe(1)
    expect(envelopeAt(pad, 0.03 + 0.05)).toBeCloseTo(Math.sqrt(DECAY_FLOOR), 6)
    expect(envelopeAt(pad, 0.2)).toBe(0)
  })

  test('no decay plays the region out at full level', () => {
    const pad = { ...defaultPad(0), attack: 1, decay: 0 }
    expect(envelopeAt(pad, 5)).toBe(1)
  })
})

describe('dropping files', () => {
  test('several files fill consecutive pads and stop at the last one', () => {
    expect(dropPads(4, 1)).toEqual([4])
    expect(dropPads(4, 3)).toEqual([4, 5, 6])
    expect(dropPads(PAD_COUNT - 2, 5)).toEqual([PAD_COUNT - 2, PAD_COUNT - 1])
    expect(dropPads(0, 0)).toEqual([])
  })
})
