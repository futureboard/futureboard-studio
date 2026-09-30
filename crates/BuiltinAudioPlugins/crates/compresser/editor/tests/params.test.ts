/**
 * The editor duplicates a handful of Rust constants — the wire ids, ranges,
 * defaults and the gain curve. Duplication is unavoidable (the editor is a
 * separate bundle), so these tests read the real `.rs` files and compare,
 * which turns a silent drift into a failing check. Run with `bun test`.
 */

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { parseParams } from '../src/bridge'
import {
  BAND_COUNT,
  DEFAULT_BANDS,
  DEFAULT_CROSSOVERS_HZ,
  DEFAULT_PARAMS,
  MAX_ATTACK_MS,
  MAX_CROSSOVER_HZ,
  MAX_KNEE_DB,
  MAX_MAKEUP_DB,
  MAX_OUTPUT_DB,
  MAX_RATIO,
  MAX_RELEASE_MS,
  MAX_SIDECHAIN_HZ,
  MAX_THRESHOLD_DB,
  MIN_ATTACK_MS,
  MIN_CROSSOVER_HZ,
  MIN_MAKEUP_DB,
  MIN_OUTPUT_DB,
  MIN_RATIO,
  MIN_RELEASE_MS,
  MIN_THRESHOLD_DB,
  PARAM_IDS,
  SIDECHAIN_OFF_HZ,
  SOLO_NONE,
  crossoverBounds,
  curveReductionDb,
  hzToUnit,
  logTravel,
  unitToHz,
} from '../src/lib/params'

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

function rustBlock(source: string, start: string, end: string): string {
  const from = source.indexOf(start)
  if (from < 0) throw new Error(`${start} not found in Rust source`)
  const to = source.indexOf(end, from)
  return source.slice(from, to)
}

const rustNumber = (text: string) => Number(text.replace(/_/g, ''))

/// `field: value` pairs from a Rust struct literal, in source order.
function rustFields(block: string): [string, string][] {
  return Array.from(block.matchAll(/(\w+):\s*(-?[0-9_.]+|true|false|Mode::\w+|SOLO_NONE)\s*,/g), (m) => [
    m[1]!,
    m[2]!,
  ])
}

const camel = (snake: string) => snake.replace(/_([a-z])/g, (_, c: string) => c.toUpperCase())

describe('the editor mirrors compresser::ipc and compresser', () => {
  test('wire ids match UI_PARAM_IDS in order', () => {
    const table = rustBlock(IPC_RS, 'pub const UI_PARAM_IDS', '];')
    const ids = Array.from(table.matchAll(/"([^"]*)"/g), (m) => m[1])
    expect([...PARAM_IDS]).toEqual(ids)
  })

  test('ranges match the Rust constants', () => {
    const pairs: [number, string][] = [
      [MIN_CROSSOVER_HZ, 'MIN_CROSSOVER_HZ'],
      [MAX_CROSSOVER_HZ, 'MAX_CROSSOVER_HZ'],
      [MIN_THRESHOLD_DB, 'MIN_THRESHOLD_DB'],
      [MAX_THRESHOLD_DB, 'MAX_THRESHOLD_DB'],
      [MIN_RATIO, 'MIN_RATIO'],
      [MAX_RATIO, 'MAX_RATIO'],
      [MAX_KNEE_DB, 'MAX_KNEE_DB'],
      [MIN_ATTACK_MS, 'MIN_ATTACK_MS'],
      [MAX_ATTACK_MS, 'MAX_ATTACK_MS'],
      [MIN_RELEASE_MS, 'MIN_RELEASE_MS'],
      [MAX_RELEASE_MS, 'MAX_RELEASE_MS'],
      [MIN_MAKEUP_DB, 'MIN_MAKEUP_DB'],
      [MAX_MAKEUP_DB, 'MAX_MAKEUP_DB'],
      [MIN_OUTPUT_DB, 'MIN_OUTPUT_DB'],
      [MAX_OUTPUT_DB, 'MAX_OUTPUT_DB'],
      [SIDECHAIN_OFF_HZ, 'SIDECHAIN_OFF_HZ'],
      [MAX_SIDECHAIN_HZ, 'MAX_SIDECHAIN_HZ'],
      [SOLO_NONE, 'SOLO_NONE'],
      [BAND_COUNT, 'BAND_COUNT'],
    ]
    for (const [value, name] of pairs) expect({ name, value }).toEqual({ name, value: rustScalar(LIB_RS, name) })
    const array = /pub const DEFAULT_CROSSOVERS_HZ\s*:[^=]+=\s*\[([\s\S]*?)\];/.exec(LIB_RS)?.[1]
    if (!array) throw new Error('DEFAULT_CROSSOVERS_HZ not found in Rust source')
    const crossovers = array
      .split(',')
      .map((part) => part.trim())
      .filter(Boolean)
      .map(rustNumber)
    expect([...DEFAULT_CROSSOVERS_HZ]).toEqual(crossovers)
  })

  test('default_params() matches DEFAULT_PARAMS', () => {
    const body = rustBlock(LIB_RS, 'pub fn default_params()', '\n}\n')
    for (const [field, raw] of rustFields(body)) {
      const key = camel(field) as keyof typeof DEFAULT_PARAMS
      const actual = DEFAULT_PARAMS[key]
      if (raw === 'true' || raw === 'false') expect({ key, actual }).toEqual({ key, actual: raw === 'true' })
      else if (raw.startsWith('Mode::')) expect(actual).toBe(raw.slice(6).toLowerCase())
      else if (raw === 'SOLO_NONE') expect(actual).toBe(SOLO_NONE)
      else expect({ key, actual }).toEqual({ key, actual: rustNumber(raw) })
    }
  })

  test('DEFAULT_BANDS matches the Rust table band for band', () => {
    const table = rustBlock(LIB_RS, 'pub const DEFAULT_BANDS', '];')
    const bands = table.split('Band {').slice(1)
    expect(bands.length).toBe(BAND_COUNT)
    bands.forEach((literal, index) => {
      const expected = Object.fromEntries(
        rustFields(literal).map(([field, raw]) => [
          camel(field),
          raw === 'true' || raw === 'false' ? raw === 'true' : rustNumber(raw),
        ]),
      )
      expect(DEFAULT_BANDS[index]).toEqual(expected as never)
    })
  })

  test('the drawn curve is the DSP curve', () => {
    // The same points `compresser::tests` pins on the Rust side.
    expect(curveReductionDb(-30, -20, 4, 6)).toBe(0)
    expect(curveReductionDb(-8, -20, 4, 6)).toBeCloseTo(9, 5)
    expect(curveReductionDb(-23, -20, 4, 6)).toBeCloseTo(0, 6)
    expect(curveReductionDb(-17, -20, 4, 6)).toBeCloseTo(2.25, 5)
    expect(curveReductionDb(-20, -20, 4, 6)).toBeCloseTo(0.5625, 5)
    expect(curveReductionDb(-20, -20, 4, 0)).toBe(0)
    expect(curveReductionDb(0, -20, 1, 6)).toBe(0)
  })
})

describe('state parsing', () => {
  const blob = () => ({
    version: 1,
    params: {
      power: true,
      mode: 'multi',
      thresholdDb: -24,
      ratio: 6,
      attackMs: 3,
      releaseMs: 250,
      makeupDb: 2,
      sidechainHpfHz: 120,
      kneeDb: 3,
      mix: 80,
      outputDb: -1,
      crossoverHz: [150, 1_200, 6_000],
      bands: DEFAULT_BANDS.map((band) => ({ ...band })),
      soloBand: 2,
    },
  })

  test('a Rust state blob parses', () => {
    const parsed = parseParams(blob())
    expect(parsed?.mode).toBe('multi')
    expect(parsed?.crossoverHz).toEqual([150, 1_200, 6_000])
    expect(parsed?.bands[3]).toEqual({ ...DEFAULT_BANDS[3]! })
    expect(parsed?.soloBand).toBe(2)
  })

  test('a partial or malformed blob is rejected whole', () => {
    const missing = blob() as { params: Record<string, unknown> }
    delete missing.params.bands
    expect(parseParams(missing)).toBeNull()
    const badMode = blob()
    badMode.params.mode = 'triple'
    expect(parseParams(badMode)).toBeNull()
    const badBand = blob()
    ;(badBand.params.bands[1] as Record<string, unknown>).bypass = 'yes'
    expect(parseParams(badBand)).toBeNull()
    expect(parseParams(null)).toBeNull()
  })

  test('out-of-range values are clamped, a bad solo means none', () => {
    const wild = blob()
    wild.params.ratio = 500
    wild.params.soloBand = 9
    wild.params.bands[0]!.thresholdDb = -900
    const parsed = parseParams(wild)
    expect(parsed?.ratio).toBe(MAX_RATIO)
    expect(parsed?.soloBand).toBe(SOLO_NONE)
    expect(parsed?.bands[0]?.thresholdDb).toBe(MIN_THRESHOLD_DB)
  })
})

describe('axes and travel', () => {
  test('the frequency axis round-trips', () => {
    for (const hz of [20, 120, 1_000, 5_000, 20_000]) expect(unitToHz(hzToUnit(hz))).toBeCloseTo(hz, 3)
  })

  test('a crossover cannot pass its neighbours', () => {
    const [lo, hi] = crossoverBounds([120, 1_000, 5_000], 1)
    expect(lo).toBeGreaterThan(120)
    expect(hi).toBeLessThan(5_000)
  })

  test('log travel spans the whole range', () => {
    const travel = logTravel(MIN_ATTACK_MS, MAX_ATTACK_MS)
    expect(travel.fromProgress(0)).toBeCloseTo(MIN_ATTACK_MS, 6)
    expect(travel.fromProgress(1)).toBeCloseTo(MAX_ATTACK_MS, 6)
    expect(travel.toProgress(travel.fromProgress(0.37))).toBeCloseTo(0.37, 6)
  })
})
