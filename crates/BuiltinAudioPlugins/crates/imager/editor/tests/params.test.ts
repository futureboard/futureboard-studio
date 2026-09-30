/**
 * The editor duplicates a handful of Rust constants — the wire ids, ranges and
 * defaults. Duplication is unavoidable (the editor is a separate bundle), so
 * these tests read the real `.rs` files and compare, which turns a silent
 * drift into a failing check. Run with `bun test`.
 */

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { parseParams } from '../src/bridge'
import {
  BAND_COUNT,
  DEFAULT_CROSSOVERS_HZ,
  DEFAULT_PARAMS,
  DEFAULT_WIDTH,
  MAX_CROSSOVER_HZ,
  MAX_OUTPUT_DB,
  MAX_WIDTH,
  MIN_CROSSOVER_HZ,
  MIN_OUTPUT_DB,
  PARAM_IDS,
  SOLO_NONE,
  bandAt,
  crossoverBounds,
  hzToUnit,
  unitToHz,
} from '../src/lib/params'
import { FACTORY_PRESETS, matchingPresetIndex } from '../src/lib/presets'

const read = (relative: string) =>
  readFileSync(fileURLToPath(new URL(relative, import.meta.url)), 'utf8')

const LIB_RS = read('../../src/lib.rs')
const IPC_RS = read('../../src/ipc.rs')

function rustScalar(source: string, name: string): number {
  const match = new RegExp(`pub const ${name}\\s*:\\s*(?:f32|i32|usize)\\s*=\\s*(-?[0-9_.]+)\\s*;`).exec(
    source,
  )
  if (!match?.[1]) throw new Error(`${name} not found in Rust source`)
  return Number(match[1].replace(/_/g, ''))
}

function rustArray(source: string, name: string): string {
  const match = new RegExp(`pub const ${name}\\s*:[^=]+=\\s*\\[([\\s\\S]*?)\\];`).exec(source)
  if (!match?.[1]) throw new Error(`${name} not found in Rust source`)
  return match[1]
}

describe('the editor mirrors imager::ipc and imager', () => {
  test('wire ids match UI_PARAM_IDS in order', () => {
    const ids = Array.from(rustArray(IPC_RS, 'UI_PARAM_IDS').matchAll(/"([^"]*)"/g), (m) => m[1])
    expect([...PARAM_IDS]).toEqual(ids)
  })

  test('ranges and defaults match the Rust constants', () => {
    expect(MIN_CROSSOVER_HZ).toBe(rustScalar(LIB_RS, 'MIN_CROSSOVER_HZ'))
    expect(MAX_CROSSOVER_HZ).toBe(rustScalar(LIB_RS, 'MAX_CROSSOVER_HZ'))
    expect(MAX_WIDTH).toBe(rustScalar(LIB_RS, 'MAX_WIDTH'))
    expect(DEFAULT_WIDTH).toBe(rustScalar(LIB_RS, 'DEFAULT_WIDTH'))
    expect(MIN_OUTPUT_DB).toBe(rustScalar(LIB_RS, 'MIN_OUTPUT_DB'))
    expect(MAX_OUTPUT_DB).toBe(rustScalar(LIB_RS, 'MAX_OUTPUT_DB'))
    expect(SOLO_NONE).toBe(rustScalar(LIB_RS, 'SOLO_NONE'))
    expect(BAND_COUNT).toBe(rustScalar(LIB_RS, 'BAND_COUNT'))
    const crossovers = rustArray(LIB_RS, 'DEFAULT_CROSSOVERS_HZ')
      .split(',')
      .map((part) => part.trim())
      .filter(Boolean)
      .map((part) => Number(part.replace(/_/g, '')))
    expect([...DEFAULT_CROSSOVERS_HZ]).toEqual(crossovers)
  })
})

describe('state parsing', () => {
  test('a Rust state blob parses', () => {
    const parsed = parseParams({
      version: 1,
      params: {
        power: true,
        crossoverHz: [150, 1500, 8000],
        width: [0, 100, 150, 200],
        soloBand: 2,
        outputDb: -3,
      },
    })
    expect(parsed).toEqual({
      power: true,
      crossoverHz: [150, 1500, 8000],
      width: [0, 100, 150, 200],
      soloBand: 2,
      outputDb: -3,
    })
  })

  test('a partial or malformed blob is rejected whole', () => {
    expect(parseParams(null)).toBeNull()
    expect(parseParams({ params: { power: true } })).toBeNull()
    expect(
      parseParams({ power: true, crossoverHz: [1, 2], width: [1, 2, 3, 4], soloBand: -1, outputDb: 0 }),
    ).toBeNull()
  })

  test('an out-of-table solo reads as no solo', () => {
    const parsed = parseParams({
      power: true,
      crossoverHz: [120, 1000, 6000],
      width: [100, 100, 100, 100],
      soloBand: 9,
      outputDb: 0,
    })
    expect(parsed?.soloBand).toBe(SOLO_NONE)
  })
})

describe('band geometry', () => {
  test('the frequency axis round-trips', () => {
    for (const hz of [20, 100, 1000, 6000, 20000]) {
      expect(unitToHz(hzToUnit(hz))).toBeCloseTo(hz, 3)
    }
  })

  test('a crossover cannot be dragged past its neighbours', () => {
    const [lo, hi] = crossoverBounds([120, 1000, 6000], 1)
    expect(lo).toBeGreaterThan(120)
    expect(hi).toBeLessThan(6000)
    expect(crossoverBounds([120, 1000, 6000], 0)[0]).toBe(MIN_CROSSOVER_HZ)
    expect(crossoverBounds([120, 1000, 6000], 2)[1]).toBe(MAX_CROSSOVER_HZ)
  })

  test('frequencies land in the band the DSP puts them in', () => {
    const sorted = [120, 1000, 6000]
    expect(bandAt(sorted, 50)).toBe(0)
    expect(bandAt(sorted, 500)).toBe(1)
    expect(bandAt(sorted, 3000)).toBe(2)
    expect(bandAt(sorted, 12000)).toBe(3)
  })
})

describe('factory presets', () => {
  test('every preset is inside the ranges Rust clamps to', () => {
    for (const { name, params } of FACTORY_PRESETS) {
      expect(params.width, name).toHaveLength(BAND_COUNT)
      for (const width of params.width) {
        expect(width).toBeGreaterThanOrEqual(0)
        expect(width).toBeLessThanOrEqual(MAX_WIDTH)
      }
      for (const hz of params.crossoverHz) {
        expect(hz).toBeGreaterThanOrEqual(MIN_CROSSOVER_HZ)
        expect(hz).toBeLessThanOrEqual(MAX_CROSSOVER_HZ)
      }
      expect(params.soloBand).toBe(SOLO_NONE)
      expect(params.power).toBe(true)
    }
  })

  test('each preset is recognised as itself, and the defaults as Default', () => {
    FACTORY_PRESETS.forEach((entry, index) => {
      expect(matchingPresetIndex(entry.params)).toBe(index)
    })
    expect(matchingPresetIndex(DEFAULT_PARAMS)).toBe(0)
  })
})
