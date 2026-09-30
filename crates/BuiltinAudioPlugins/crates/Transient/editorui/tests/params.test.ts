/**
 * The editor duplicates the Rust ids, ranges, defaults and shaping depth.
 * These tests read the real `.rs` files and compare, so a silent drift fails
 * here instead of shipping an editor that disagrees with the DSP.
 */

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { DEFAULT_PARAMS, MAX_SHAPE_DB, PARAM_IDS, RANGES, describeShape, parseParams, wireValues } from '../src/lib/params'
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

describe('the editor mirrors transient::ipc and transient', () => {
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

  test('defaults and shaping depth match the Rust source', () => {
    const body = /pub fn default_params\(\)[\s\S]*?\n\}/.exec(LIB_RS)![0]
    for (const [, field, raw] of body.matchAll(/(\w+):\s*([-0-9_.]+|true|false),/g)) {
      const key = camel(field!) as keyof typeof DEFAULT_PARAMS
      const expected = raw === 'true' ? true : raw === 'false' ? false : num(raw!)
      expect({ key, value: DEFAULT_PARAMS[key] }).toEqual({ key, value: expected })
    }
    expect(MAX_SHAPE_DB).toBe(num(/const MAX_SHAPE_DB:\s*f32\s*=\s*([0-9_.]+);/.exec(LIB_RS)![1]!))
  })
})

describe('state, presets and readouts', () => {
  test('a Rust blob parses and clamps; a partial one is rejected', () => {
    expect(parseParams({ version: 1, params: { ...DEFAULT_PARAMS, attack: 250 } })).toEqual({
      ...DEFAULT_PARAMS,
      attack: 100,
    })
    const { speed: _speed, ...partial } = DEFAULT_PARAMS
    expect(parseParams({ params: partial })).toBeNull()
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

  test('the shape readout is the dB at the peak', () => {
    expect(describeShape(0)).toBe('Unchanged')
    expect(describeShape(100)).toBe('+18.0 dB')
    expect(describeShape(-50)).toBe('−9.0 dB')
  })
})
