/**
 * The editor duplicates the Rust ids, ranges, defaults and style order.
 * These tests read the real `.rs` files and compare, so a silent drift fails
 * here instead of shipping an editor that disagrees with the DSP.
 */

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import {
  DEFAULT_PARAMS,
  PARAM_IDS,
  RANGES,
  STYLES,
  parseParams,
  styleFromWire,
  styleToWire,
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

describe('the editor mirrors burnlimit::ipc and burnlimit', () => {
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
    for (const [, field, raw] of body.matchAll(/(\w+):\s*([-0-9_.]+|true|false|Style::\w+),/g)) {
      const key = camel(field!) as keyof typeof DEFAULT_PARAMS
      const expected = raw === 'true' ? true : raw === 'false' ? false : raw!.startsWith('Style::') ? raw!.slice(7).toLowerCase() : num(raw!)
      expect({ key, value: DEFAULT_PARAMS[key] }).toEqual({ key, value: expected })
    }
  })

  test('style order is the enum order', () => {
    const body = /pub enum Style \{([\s\S]*?)\n\}/.exec(LIB_RS)![1]!
    expect([...STYLES]).toEqual(Array.from(body.matchAll(/^\s{4}(\w+),/gm), (m) => m[1]!.toLowerCase()))
    for (const style of STYLES) expect(styleFromWire(styleToWire(style))).toBe(style)
    expect(styleFromWire(9)).toBe('clean')
  })
})

describe('state and presets', () => {
  test('a Rust blob parses and clamps; a partial one is rejected', () => {
    const blob = { version: 1, params: { ...DEFAULT_PARAMS, style: 'clip', gainDb: 99 } }
    expect(parseParams(blob)).toEqual({ ...DEFAULT_PARAMS, style: 'clip', gainDb: 24 })
    const { mix: _mix, ...partial } = DEFAULT_PARAMS
    expect(parseParams({ params: partial })).toBeNull()
    expect(parseParams({ params: { ...DEFAULT_PARAMS, style: 'loud' } })).toBeNull()
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
