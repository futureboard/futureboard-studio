/**
 * Editor-side mirror of the Drum Sampler parameter contract.
 *
 * Rust owns the authority: `src/ipc.rs` defines the ids, ranges and clamping,
 * `src/lib.rs` the defaults and the DSP. This module exists so the editor can
 * lay out a control, format a readout and draw the envelope the DSP will play
 * — never to decide what a value means. `tests/pads.test.ts` pins the numbers
 * here against the Rust source.
 */

export const PAD_COUNT = 16

/// The pads a drop of `count` files onto pad `first` fills, one file each in
/// order, stopping at the last pad.
export function dropPads(first: number, count: number): number[] {
  const last = Math.min(PAD_COUNT, first + Math.max(0, count))
  return Array.from({ length: Math.max(0, last - first) }, (_, offset) => first + offset)
}

export const FILTER_MODES = ['off', 'lowPass', 'highPass', 'bandPass'] as const
export type FilterMode = (typeof FILTER_MODES)[number]
export const FILTER_LABELS: Record<FilterMode, string> = {
  off: 'Off',
  lowPass: 'LP',
  highPass: 'HP',
  bandPass: 'BP',
}

export type Pad = {
  note: number
  tune: number
  gain: number
  pan: number
  choke: number
  attack: number
  /// Legacy: stored and round-tripped, never played (see `Pad::release_ms`).
  release: number
  reverse: boolean
  mute: boolean
  solo: boolean
  sampleName: string | null
  start: number
  end: number
  filterMode: FilterMode
  cutoff: number
  resonance: number
  velocity: number
  hold: number
  decay: number
}

export type Kit = {
  pads: Pad[]
  masterGain: number
  masterTune: number
}

/// Every editable pad field and the suffix of its wire id (`pad3Gain`).
/// Order is irrelevant here — the Rust table fixes the indices.
export const FIELD_SUFFIX = {
  note: 'Note',
  tune: 'Tune',
  gain: 'Gain',
  pan: 'Pan',
  choke: 'Choke',
  attack: 'Attack',
  release: 'Release',
  reverse: 'Reverse',
  mute: 'Mute',
  solo: 'Solo',
  start: 'Start',
  end: 'End',
  filterMode: 'FilterMode',
  cutoff: 'Cutoff',
  resonance: 'Resonance',
  velocity: 'Velocity',
  hold: 'Hold',
  decay: 'Decay',
} as const

export type PadField = keyof typeof FIELD_SUFFIX

export const padParamId = (pad: number, field: PadField) => `pad${pad}${FIELD_SUFFIX[field]}`

/// Ranges, mirroring `ipc::apply_wire_param`.
export const RANGE = {
  tune: [-24, 24],
  gain: [-60, 12],
  pan: [-1, 1],
  choke: [0, 8],
  attack: [0, 250],
  cutoff: [20, 20_000],
  resonance: [0, 100],
  velocity: [0, 100],
  hold: [0, 5_000],
  decay: [0, 10_000],
  masterGain: [-60, 12],
  masterTune: [-24, 24],
} as const

/// `drumsampler::MIN_REGION`.
export const MIN_REGION = 0.001

export function defaultPad(index: number): Pad {
  return {
    note: 36 + index,
    tune: 0,
    gain: 0,
    pan: 0,
    choke: 0,
    attack: 1,
    release: 60,
    reverse: false,
    mute: false,
    solo: false,
    sampleName: null,
    start: 0,
    end: 1,
    filterMode: 'off',
    cutoff: 20_000,
    resonance: 0,
    velocity: 100,
    hold: 0,
    decay: 0,
  }
}

export function defaultKit(): Kit {
  return {
    pads: Array.from({ length: PAD_COUNT }, (_, index) => defaultPad(index)),
    masterGain: 0,
    masterTune: 0,
  }
}

export function clamp(value: number, min: number, max: number) {
  return value < min ? min : value > max ? max : value
}

const num = (value: unknown, fallback: number) =>
  typeof value === 'number' && Number.isFinite(value) ? value : fallback
const bool = (value: unknown, fallback: boolean) => (typeof value === 'boolean' ? value : fallback)

/// Read a host state blob. Fields a first-release project does not carry fall
/// back to the behaviour it had then — the same serde defaults Rust applies —
/// so an older kit opens looking the way it sounds.
export function parseKit(state: unknown): Kit | null {
  if (!state || typeof state !== 'object') return null
  const candidate = 'params' in state ? (state as { params?: unknown }).params : state
  if (!candidate || typeof candidate !== 'object') return null
  const raw = candidate as Record<string, unknown>
  if (!Array.isArray(raw.pads) || raw.pads.length !== PAD_COUNT) return null
  const pads = raw.pads.map((entry, index): Pad => {
    const source = (entry ?? {}) as Record<string, unknown>
    const d = defaultPad(index)
    const mode = FILTER_MODES.includes(source.filterMode as FilterMode)
      ? (source.filterMode as FilterMode)
      : d.filterMode
    return {
      note: num(source.note, d.note),
      tune: num(source.tuneSemitones, d.tune),
      gain: num(source.gainDb, d.gain),
      pan: num(source.pan, d.pan),
      choke: num(source.chokeGroup, d.choke),
      attack: num(source.attackMs, d.attack),
      release: num(source.releaseMs, d.release),
      reverse: bool(source.reverse, d.reverse),
      mute: bool(source.muted, d.mute),
      solo: bool(source.solo, d.solo),
      sampleName: typeof source.sampleName === 'string' ? source.sampleName : null,
      start: num(source.start, d.start),
      end: num(source.end, d.end),
      filterMode: mode,
      cutoff: num(source.cutoffHz, d.cutoff),
      resonance: num(source.resonance, d.resonance),
      velocity: num(source.velocitySensitivity, d.velocity),
      hold: num(source.holdMs, d.hold),
      decay: num(source.decayMs, d.decay),
    }
  })
  return { pads, masterGain: num(raw.masterGainDb, 0), masterTune: num(raw.masterTune, 0) }
}

/// Wire value for a field: booleans as 0/1, the filter mode as its index.
export function wireValue(field: PadField, value: Pad[PadField]): number {
  if (typeof value === 'boolean') return value ? 1 : 0
  if (field === 'filterMode') return FILTER_MODES.indexOf(value as FilterMode)
  return value as number
}

const NOTE_NAMES = ['C', 'C#', 'D', 'D#', 'E', 'F', 'F#', 'G', 'G#', 'A', 'A#', 'B']

export function noteName(note: number): string {
  return `${NOTE_NAMES[((note % 12) + 12) % 12]}${Math.floor(note / 12) - 1}`
}

export const formatDb = (db: number) => `${db > 0 ? '+' : ''}${db.toFixed(1)}`
export const formatSemis = (st: number) => `${st > 0 ? '+' : ''}${Math.round(st * 10) / 10}`
export const formatPan = (pan: number) =>
  Math.abs(pan) < 0.005 ? 'C' : pan < 0 ? `L${Math.round(-pan * 100)}` : `R${Math.round(pan * 100)}`
export const formatPercent = (value: number) => `${Math.round(value)}`
export function formatMs(ms: number): string {
  if (ms >= 1000) return `${(ms / 1000).toFixed(ms >= 10_000 ? 1 : 2)} s`
  return `${Math.round(ms)} ms`
}
export function formatHz(hz: number): string {
  return hz >= 1000 ? `${(hz / 1000).toFixed(hz >= 10_000 ? 1 : 2)}k` : `${Math.round(hz)}`
}

/// The region the DSP plays: sorted and never narrower than `MIN_REGION`,
/// as `drumsampler::region_frames` does.
export function effectiveRegion(pad: Pad): [number, number] {
  let lo = clamp(Math.min(pad.start, pad.end), 0, 1)
  let hi = clamp(Math.max(pad.start, pad.end), 0, 1)
  if (hi - lo < MIN_REGION) {
    hi = Math.min(lo + MIN_REGION, 1)
    lo = hi - MIN_REGION
  }
  return [lo, hi]
}

/// Seconds the region lasts at the pad's (and the kit's) tune.
export function regionSeconds(pad: Pad, frames: number, sampleRate: number, masterTune: number): number {
  if (!(frames > 0 && sampleRate > 0)) return 0
  const [lo, hi] = effectiveRegion(pad)
  return ((hi - lo) * frames) / sampleRate / Math.pow(2, (pad.tune + masterTune) / 12)
}

/// `drumsampler::DECAY_FLOOR` — the level a decay time is measured to.
export const DECAY_FLOOR = 0.001

/// Envelope level at `t` seconds after the trigger: linear attack, hold, and
/// an exponential decay reaching −60 dB after `decay` ms — the same shape
/// `Voice::envelope` plays. A pad with no decay holds full level.
export function envelopeAt(pad: Pad, t: number): number {
  const attack = Math.max(pad.attack, 0) / 1000
  if (t < attack) return attack > 0 ? t / attack : 1
  if (pad.decay <= 0) return 1
  const hold = Math.max(pad.hold, 0) / 1000
  if (t < attack + hold) return 1
  const level = Math.pow(DECAY_FLOOR, (t - attack - hold) / (pad.decay / 1000))
  return level <= DECAY_FLOOR ? 0 : level
}

/// Filter resonance → Q, as `SvfCoeffs::for_pad` maps it.
export function resonanceQ(resonance: number): number {
  const minQ = Math.SQRT1_2
  return minQ * Math.pow(12 / minQ, clamp(resonance, 0, 100) / 100)
}

/// Magnitude of the pad's filter at `hz` (analogue state-variable prototype —
/// the DSP's TPT form matches it closely below a few kHz of Nyquist).
export function filterMagnitude(pad: Pad, hz: number): number {
  if (pad.filterMode === 'off') return 1
  const w = hz / Math.max(pad.cutoff, 1)
  const k = 1 / resonanceQ(pad.resonance)
  const re = 1 - w * w
  const im = k * w
  const den = Math.sqrt(re * re + im * im)
  switch (pad.filterMode) {
    case 'lowPass':
      return 1 / den
    case 'highPass':
      return (w * w) / den
    case 'bandPass':
      return w / den
  }
}
