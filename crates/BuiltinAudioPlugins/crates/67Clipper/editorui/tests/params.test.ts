/**
 * The editor duplicates the Rust ids, ranges, defaults and mode order. These
 * tests read the real `.rs` files and compare, so a silent drift fails here
 * instead of shipping an editor that disagrees with the DSP.
 */

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import {
  DEFAULT_PARAMS,
  MODES,
  PARAM_IDS,
  RANGES,
  modeFromWire,
  modeToWire,
  parseParams,
  wireValues,
} from '../src/lib/params'
import { FACTORY_PRESETS, matchingPresetIndex } from '../src/lib/presets'

const read = (relative: string) => readFileSync(fileURLToPath(new URL(relative, import.meta.url)), 'utf8')
const LIB_RS = read('../../src/lib.rs')
const IPC_RS = read('../../src/ipc.rs')

const arrayBody = (source: string, name: string) => {
  const match = new RegExp(`const ${name}\\s*:[^=]+=\\s*\\[([\\s\\S]*?)\\];`).exec(source)
  if (!match?.[1]) throw new Error(`${name} not found`)
  return match[1]
}
const num = (text: string) => Number(text.replace(/_/g, ''))
const camel = (snake: string) => snake.replace(/_([a-z])/g, (_, c: string) => c.toUpperCase())

describe('the editor mirrors clipper67::ipc and clipper67', () => {
  test('wire ids in order', () => {
    expect([...PARAM_IDS]).toEqual(Array.from(arrayBody(IPC_RS, 'UI_PARAM_IDS').matchAll(/"([^"]*)"/g), (m) => m[1]))
    expect(wireValues(DEFAULT_PARAMS).map(([id]) => id)).toEqual([...PARAM_IDS])
  })

  test('ranges match RANGES', () => {
    const rust = Array.from(arrayBody(IPC_RS, 'RANGES').matchAll(/\(\s*([-0-9_.]+)\s*,\s*([-0-9_.]+)\s*\),\s*\/\/\s*(\w+)/g))
    for (const [, min, max, id] of rust) {
      const range = RANGES[id as keyof typeof RANGES]
      if (range) expect({ id, range: [...range] }).toEqual({ id, range: [num(min!), num(max!)] })
    }
    expect(Object.keys(RANGES).length).toBe(rust.filter(([, min, max]) => min !== max).length)
  })

  test('defaults match default_params()', () => {
    const body = /pub fn default_params\(\)[\s\S]*?\n\}/.exec(LIB_RS)![0]
    for (const [, field, raw] of body.matchAll(/(\w+):\s*([-0-9_.]+|true|false|Mode::\w+),/g)) {
      const key = camel(field!) as keyof typeof DEFAULT_PARAMS
      const expected = raw === 'true' ? true : raw === 'false' ? false : raw!.startsWith('Mode::') ? raw!.slice(6).toLowerCase() : num(raw!)
      expect({ key, value: DEFAULT_PARAMS[key] }).toEqual({ key, value: expected })
    }
  })

  test('mode order is the enum order', () => {
    const body = /pub enum Mode \{([\s\S]*?)\n\}/.exec(LIB_RS)![1]!
    expect([...MODES]).toEqual(Array.from(body.matchAll(/^\s{4}(\w+),/gm), (m) => m[1]!.toLowerCase()))
    for (const mode of MODES) expect(modeFromWire(modeToWire(mode))).toBe(mode)
    expect(modeFromWire(9)).toBe('clip')
  })
})

describe('state and presets', () => {
  test('a Rust blob parses and clamps; a partial one is rejected', () => {
    const blob = { version: 1, params: { ...DEFAULT_PARAMS, mode: 'limit', thresholdDb: -99 } }
    expect(parseParams(blob)).toEqual({ ...DEFAULT_PARAMS, mode: 'limit', thresholdDb: -24 })
    const { dcFilter: _dc, ...partial } = DEFAULT_PARAMS
    expect(parseParams({ params: partial })).toBeNull()
    expect(parseParams({ params: { ...DEFAULT_PARAMS, mode: 'fold' } })).toBeNull()
  })

  test('every preset is in range and matches itself', () => {
    FACTORY_PRESETS.forEach((preset, index) => {
      expect(matchingPresetIndex(preset.params)).toBe(index)
      for (const [id, [min, max]] of Object.entries(RANGES)) {
        const value = preset.params[id as keyof typeof RANGES]
        expect(value >= min && value <= max).toBe(true)
      }
    })
  })
})
