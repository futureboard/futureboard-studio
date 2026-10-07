// Rodhareist's editor: a port of the native rodhareist_panel.rs over the
// block model in rodhareist_blocks.rs, editing the way rodhareist_window.rs
// does. A preset bar (name, trims, tempo, power), the factory bank, the signal
// path between the INPUT and MAIN meters, and the block editor (category →
// model → sliders).
//
// It draws its own frame rather than EditorShell: the native editor has its
// own preset bar and bank list in place of plugin_kit's header (no A/B).
//
// Loading a cabinet IR or a NAM capture opens a file on the machine running
// the engine through Studio's dialogs and host ops; a web page cannot, so
// those two loaders are a note here. Everything else is a wire parameter.

import { useRef, useState } from 'react'
import type { CSSProperties, KeyboardEvent, PointerEvent, ReactNode } from 'react'
import { ChevronLeft, ChevronRight, Power, X } from 'lucide-react'
import type { Editor, EditorComponent } from './kit.tsx'
import { LiveCanvas } from './kit.tsx'
import type { Ctx } from './paint.ts'
import { colors, label as paintLabel, rect } from './paint.ts'
import './rodhareist.css'

// ── The block model (rodhareist_blocks.rs) ──────────────────────────────

const PATH_SLOTS = 15

/** `StageKind` discriminants: the `path_slot_*` wire values. */
const Stage = {
  Gate: 0,
  Drive: 1,
  Amp: 2,
  Mod: 3,
  Delay: 4,
  Reverb: 5,
  Cab: 6,
  Comp: 7,
  Eq: 8,
  Wah: 9,
  Drive2: 10,
  Mod2: 11,
  Delay2: 12,
  Eq2: 13,
  Comp2: 14,
} as const
const STAGE_COUNT = 15

/** The plug-in's own identity colours (the native `pal`), kept as they are:
 *  amber for dirt and dynamics before the amp, teal for time and
 *  modulation, neutral for tone shaping. */
const AMBER = '#e7a838'
const TINT = { amber: AMBER, teal: '#4fc3c4', neutral: '#b9b4a8' } as const
type Tint = keyof typeof TINT

interface Category {
  name: string
  glyph: string
  tint: Tint
  /** The stages of this category, first instance first. */
  kinds: number[]
}

const CATEGORIES: Category[] = [
  { name: 'Gate', glyph: 'GTE', tint: 'amber', kinds: [Stage.Gate] },
  { name: 'Compressor', glyph: 'CMP', tint: 'amber', kinds: [Stage.Comp, Stage.Comp2] },
  { name: 'Wah', glyph: 'WAH', tint: 'amber', kinds: [Stage.Wah] },
  { name: 'Drive', glyph: 'DRV', tint: 'amber', kinds: [Stage.Drive, Stage.Drive2] },
  { name: 'Amp', glyph: 'AMP', tint: 'amber', kinds: [Stage.Amp] },
  { name: 'Cab', glyph: 'CAB', tint: 'neutral', kinds: [Stage.Cab] },
  { name: 'EQ', glyph: 'EQ', tint: 'neutral', kinds: [Stage.Eq, Stage.Eq2] },
  { name: 'Modulation', glyph: 'MOD', tint: 'teal', kinds: [Stage.Mod, Stage.Mod2] },
  { name: 'Delay', glyph: 'DLY', tint: 'teal', kinds: [Stage.Delay, Stage.Delay2] },
  { name: 'Reverb', glyph: 'REV', tint: 'teal', kinds: [Stage.Reverb] },
]

function categoryOf(kind: number): Category {
  return CATEGORIES.find((c) => c.kinds.includes(kind)) ?? CATEGORIES[0]
}

const DRIVE_MODELS = [
  'Green Screamer',
  'Minotaur Boost',
  'Rats Nest',
  'Breaker Blues',
  'Face Fuzz',
  'Centurion',
  'DS Classic',
  'Super Drive',
  'Metal Core',
  'Tight Rift',
  'Amber Crunch',
  'Copper Fuzz',
]
const AMP_MODELS = [
  'Mandarin 80',
  'Brit Plexi 100',
  'Twin Clean',
  'Top Boost',
  'Recto Modern',
  'JCM Crunch',
  'Lead Slate',
  'Bassman',
  'Overdrive Special',
  'Invader 5150',
  'Tweed Deluxe',
]
const CAB_MODELS = [
  '1960v Vintage 4x12',
  'American 2x12',
  'Tweed 1x12',
  'Modern 4x12',
  'Open Back',
  'Vintage 2x12',
  'Oversized 4x12',
  'Bass Cabinet',
  'British Stack 4x12',
  'Uberkab 4x12',
  'SLO Custom 4x12',
  'Impulse Response',
  'Modern 2x12',
  'American 1x12 Combo',
]
/** `CabModel::Ir`: convolution with a loaded file, not a modeled voicing. */
const CAB_IR = 11
const MIC_MODELS = ['Dynamic', 'Ribbon', 'Condenser']
const MOD_MODELS = [
  '70s Analog Chorus',
  'Vibe Phase 90',
  'Jet Flanger',
  'Opto Tremolo',
  'Molam Swirl',
  'Phin Vibe',
  'Khaen Swirl',
  'Bi-Lam',
  'Isan Jet',
  'Soft Phase',
  'Wide Vibe',
]
const WAH_MODELS = ['Cry Wah', 'Touch Wah']
const WAH_TOUCH = 1
const DELAY_MODELS = ['Tape Echo', 'Digital Delay', 'Analog Delay', 'Ping-Pong', 'Dual Delay']
const REVERB_MODELS = ['Studio Plate', 'Tracking Room', 'Concert Hall', 'Shimmer']
const REVERB_SHIMMER = 3
const EQ_MODELS = ['Studio EQ', 'Vintage EQ', 'Modern EQ']

/** `ToneEngineKind` wire values. */
const ENGINE_CLASSIC = 0
const ENGINE_NAM = 1

/** `(wire id, descriptor id, label)`: a second instance shares its range
 *  with the first instance's descriptor entry. */
type Row = readonly [string, string, string]

const compRows = (p: string): Row[] => [
  [`${p}_thresh`, 'comp_thresh', 'Threshold'],
  [`${p}_ratio`, 'comp_ratio', 'Ratio'],
  [`${p}_attack`, 'comp_attack', 'Attack'],
  [`${p}_release`, 'comp_release', 'Release'],
  [`${p}_makeup`, 'comp_makeup', 'Makeup'],
]
const driveRows = (p: string): Row[] => [
  [`${p}_gain`, 'drive_gain', 'Drive'],
  [`${p}_tone`, 'drive_tone', 'Tone'],
  [`${p}_level`, 'drive_level', 'Level'],
]
const eqRows = (p: string): Row[] => [
  [`${p}_low_gain`, 'eq_low_gain', 'Low'],
  [`${p}_mid1_freq`, 'eq_mid1_freq', 'Low Mid Freq'],
  [`${p}_mid1_gain`, 'eq_mid1_gain', 'Low Mid'],
  [`${p}_mid2_freq`, 'eq_mid2_freq', 'High Mid Freq'],
  [`${p}_mid2_gain`, 'eq_mid2_gain', 'High Mid'],
  [`${p}_high_gain`, 'eq_high_gain', 'High'],
]
const modRows = (p: string): Row[] => [
  [`${p}_rate`, 'chorus_rate', 'Rate'],
  [`${p}_depth`, 'chorus_depth', 'Depth'],
  [`${p}_mix`, 'chorus_mix', 'Mix'],
]
const delayRows = (p: string): Row[] => [
  [`${p}_time`, 'delay_time', 'Time'],
  [`${p}_fb`, 'delay_fb', 'Feedback'],
  [`${p}_tone`, 'delay_tone', 'Tone'],
  [`${p}_mix`, 'delay_mix', 'Mix'],
]

type Models = { fixed: string } | { wire: string; labels: string[] } | 'amp'

interface StageInfo {
  /** What a placed block goes by: its category, plus A/B where doubled. */
  name: string
  /** The block's own on/off wire id. */
  enable: string
  models: Models
  rows: Row[]
}

/** By `StageKind` discriminant. */
const STAGES: StageInfo[] = [
  { name: 'Gate', enable: 'gate_on', models: { fixed: 'Noise Gate' }, rows: [['gate_thresh', 'gate_thresh', 'Threshold']] },
  { name: 'Drive A', enable: 'drive_on', models: { wire: 'drive_model', labels: DRIVE_MODELS }, rows: driveRows('drive') },
  {
    name: 'Amp',
    enable: 'amp_on',
    models: 'amp',
    rows: [
      ['amp_gain', 'amp_gain', 'Drive'],
      ['amp_bass', 'amp_bass', 'Bass'],
      ['amp_middle', 'amp_middle', 'Mid'],
      ['amp_treble', 'amp_treble', 'Treble'],
      ['amp_presence', 'amp_presence', 'Presence'],
      ['amp_master', 'amp_master', 'Master'],
    ],
  },
  { name: 'Mod A', enable: 'mod_on', models: { wire: 'mod_model', labels: MOD_MODELS }, rows: modRows('chorus') },
  { name: 'Delay A', enable: 'delay_on', models: { wire: 'delay_model', labels: DELAY_MODELS }, rows: delayRows('delay') },
  {
    name: 'Reverb',
    enable: 'reverb_on',
    models: { wire: 'reverb_model', labels: REVERB_MODELS },
    rows: [
      ['reverb_decay', 'reverb_decay', 'Decay'],
      ['reverb_mix', 'reverb_mix', 'Mix'],
      ['reverb_shimmer', 'reverb_shimmer', 'Shimmer'],
    ],
  },
  {
    name: 'Cab',
    enable: 'cab_on',
    models: { wire: 'cab_model', labels: CAB_MODELS },
    rows: [
      ['cab_mic', 'cab_mic', 'Mic Position'],
      ['cab_dist', 'cab_dist', 'Distance'],
    ],
  },
  { name: 'Comp A', enable: 'comp_on', models: { fixed: 'Studio Compressor' }, rows: compRows('comp') },
  { name: 'EQ A', enable: 'eq_on', models: { wire: 'eq_model', labels: EQ_MODELS }, rows: eqRows('eq') },
  {
    name: 'Wah',
    enable: 'wah_on',
    models: { wire: 'wah_model', labels: WAH_MODELS },
    rows: [
      ['wah_pos', 'wah_pos', 'Position'],
      ['wah_res', 'wah_res', 'Resonance'],
      ['wah_sens', 'wah_sens', 'Sensitivity'],
    ],
  },
  { name: 'Drive B', enable: 'drive2_on', models: { wire: 'drive2_model', labels: DRIVE_MODELS }, rows: driveRows('drive2') },
  { name: 'Mod B', enable: 'mod2_on', models: { wire: 'mod2_model', labels: MOD_MODELS }, rows: modRows('chorus2') },
  { name: 'Delay B', enable: 'delay2_on', models: { wire: 'delay2_model', labels: DELAY_MODELS }, rows: delayRows('delay2') },
  { name: 'EQ B', enable: 'eq2_on', models: { wire: 'eq2_model', labels: EQ_MODELS }, rows: eqRows('eq2') },
  { name: 'Comp B', enable: 'comp2_on', models: { fixed: 'Studio Compressor' }, rows: compRows('comp2') },
]

/** The NAM capture rows: the descriptor does not list them, so their ranges
 *  are the DSP's (trim ±24 dB, mix and slim size in percent). */
const NAM_ROWS: readonly (readonly [string, string, number, number, string])[] = [
  ['nam_input_trim', 'Capture In', -24, 24, 'dB'],
  ['nam_output_trim', 'Capture Out', -24, 24, 'dB'],
  ['nam_mix', 'Capture Mix', 0, 100, '%'],
  ['nam_slim_size', 'Slim Size', 0, 100, '%'],
]

/** A model-select wire value as the DSP reads it: `from_index` falls back to
 *  the first model out of range. */
function modelIndex(value: number, count: number): number {
  const i = Math.round(value)
  return i >= 0 && i < count ? i : 0
}

function toneEngine(editor: Editor): number {
  const i = Math.round(editor.value('tone_engine'))
  return i >= 0 && i <= 2 ? i : ENGINE_CLASSIC
}

interface Choice {
  label: string
  /** Empty for a block with one fixed algorithm. */
  wire: string
  value: number
}

/** A block's model list and the selected entry. */
function models(editor: Editor, kind: number): [Choice[], number | null] {
  const m = STAGES[kind].models
  if (m === 'amp') {
    // Classic voicings, then the NAM engine: picking an amp model puts the
    // engine back on Classic (the DSP does that for `amp_model`).
    const list: Choice[] = AMP_MODELS.map((label, value) => ({ label, wire: 'amp_model', value }))
    list.push({ label: 'NAM Capture', wire: 'tone_engine', value: ENGINE_NAM })
    const engine = toneEngine(editor)
    const selected =
      engine === ENGINE_CLASSIC
        ? modelIndex(editor.value('amp_model'), AMP_MODELS.length)
        : engine === ENGINE_NAM
          ? list.length - 1
          : null
    return [list, selected]
  }
  if ('fixed' in m) return [[{ label: m.fixed, wire: '', value: 0 }], 0]
  return [
    m.labels.map((label, value) => ({ label, wire: m.wire, value })),
    modelIndex(editor.value(m.wire), m.labels.length),
  ]
}

/** The selected model's name, or null (amp with its engine bypassed). */
function modelName(editor: Editor, kind: number): string | null {
  const [list, selected] = models(editor, kind)
  return selected === null ? null : (list[selected]?.label ?? null)
}

const isOn = (editor: Editor, kind: number) => editor.flag(STAGES[kind].enable)

interface ParamSpec {
  id: string
  label: string
  min: number
  max: number
  unit: string
  /** Shown but inert for the selected model: dimmed, never hidden, so the
   *  layout does not jump. */
  inactive: boolean
}

/** A row from the plug-in's descriptor, as `describe` reads it. */
function describe(editor: Editor, id: string, descriptorId: string, label: string, inactive = false): ParamSpec | null {
  const d = editor.effect.params.find((p) => p.id === descriptorId)
  return d ? { id, label, min: d.min, max: d.max, unit: d.unit, inactive } : null
}

/** The slider rows for a block, in order. */
function params(editor: Editor, kind: number): ParamSpec[] {
  const inactive = (id: string) => {
    switch (id) {
      case 'wah_sens':
        return modelIndex(editor.value('wah_model'), WAH_MODELS.length) !== WAH_TOUCH
      case 'reverb_shimmer':
        return modelIndex(editor.value('reverb_model'), REVERB_MODELS.length) !== REVERB_SHIMMER
      case 'cab_mic':
      case 'cab_dist':
        return modelIndex(editor.value('cab_model'), CAB_MODELS.length) === CAB_IR
      default:
        return false
    }
  }
  const out = STAGES[kind].rows
    .map(([id, descriptorId, label]) => describe(editor, id, descriptorId, label, inactive(id)))
    .filter((spec): spec is ParamSpec => spec !== null)
  if (kind === Stage.Amp && toneEngine(editor) === ENGINE_NAM) {
    for (const [id, label, min, max, unit] of NAM_ROWS) out.push({ id, label, min, max, unit, inactive: false })
  }
  return out
}

/** A value as the editor prints it, with its unit. */
function formatValue(value: number, spec: ParamSpec): string {
  switch (spec.unit) {
    case 'dB':
      return spec.min < 0 && spec.max > 0 ? `${value >= 0 ? '+' : ''}${value.toFixed(1)} dB` : `${value.toFixed(1)} dB`
    case '%':
      return `${value.toFixed(0)} %`
    case 'ms':
      return value < 10 ? `${value.toFixed(1)} ms` : `${value.toFixed(0)} ms`
    case 's':
      return `${value.toFixed(1)} s`
    case 'Hz':
      return value >= 1000 ? `${(value / 1000).toFixed(2)} kHz` : `${value.toFixed(0)} Hz`
    case ':1':
      return `${value.toFixed(1)}:1`
    default:
      return value.toFixed(1)
  }
}

// ── The chain (rodhareist_panel.rs) ─────────────────────────────────────

const slotId = (slot: number) => `path_slot_${slot}`

/** The path as the DSP holds it: a slot's stage, or null. Like
 *  `sanitize_stage_order`, a stage's later duplicates read as empty. */
function readOrder(editor: Editor): (number | null)[] {
  const seen = new Set<number>()
  return Array.from({ length: PATH_SLOTS }, (_, slot) => {
    const kind = Math.round(editor.value(slotId(slot)))
    if (kind < 0 || kind >= STAGE_COUNT || seen.has(kind)) return null
    seen.add(kind)
    return kind
  })
}

/** Which chain slot the edit panel shows. */
type Focus = { block: number } | { empty: number }

const sameFocus = (a: Focus, b: Focus) =>
  'block' in a ? 'block' in b && a.block === b.block : 'empty' in b && a.empty === b.empty

/** The requested focus while it still matches the chain, else the first
 *  block (or the first slot of an empty chain): a preset that removes the
 *  focused block never leaves the editor pointing at nothing. */
function resolveFocus(order: (number | null)[], requested: Focus | null): Focus {
  if (requested && 'block' in requested && order.includes(requested.block)) return requested
  if (requested && 'empty' in requested && order[requested.empty] === null) return requested
  const first = order.find((kind) => kind !== null)
  return first != null ? { block: first } : { empty: 0 }
}

function focusSlot(order: (number | null)[], focus: Focus): number {
  if ('empty' in focus) return focus.empty
  const slot = order.indexOf(focus.block)
  return slot < 0 ? 0 : slot
}

/** The stage of `category` picking it puts in `slot`: the one already there,
 *  else the first instance not yet on the path. */
function stageFor(order: (number | null)[], slot: number, category: Category): number | null {
  const there = order[slot]
  if (there != null && categoryOf(there) === category) return there
  return category.kinds.find((kind) => !order.includes(kind)) ?? null
}

/** Sends `changes` as one edit. `amp_model` puts the DSP's engine back on
 *  Classic (`apply_to_params`), so when the amp moves and the engine should
 *  not, the engine goes out again after it. */
function applyWire(editor: Editor, changes: Record<string, number>) {
  const ampMoves = 'amp_model' in changes && Math.round(changes.amp_model) !== Math.round(editor.value('amp_model'))
  const engine = changes.tone_engine ?? editor.value('tone_engine')
  editor.setMany(changes)
  if (ampMoves && Math.round(engine) !== ENGINE_CLASSIC) editor.set('tone_engine', engine)
}

/** One wire edit, with the DSP's own side effects mirrored (`set_wire`). */
function setWire(editor: Editor, id: string, value: number) {
  if (id === 'amp_model') applyWire(editor, { amp_model: value, tone_engine: ENGINE_CLASSIC })
  else editor.set(id, value)
}

/** Path slots to rewrite, written lowest slot first: the DSP clears a
 *  stage's later duplicate after each write, so a swap lands whole. */
function pathEdit(editor: Editor, slots: [number, number | null][]) {
  const changes: Record<string, number> = {}
  for (const [slot, kind] of [...slots].sort((a, b) => a[0] - b[0])) changes[slotId(slot)] = kind ?? -1
  editor.setMany(changes)
}

// ── Presets (rodhareist_window.rs) ──────────────────────────────────────

/** Bank positions, by preset name (presets.rs): the wire spec carries names
 *  only. */
const PRESET_IDS: Record<string, string> = {
  'Studio Clean': '01A',
  'Warm Jazz': '01B',
  'Country Slapback': '01C',
  'Funk Auto-Wah': '01D',
  'Phase Funk': '01E',
  'Jangle Chorus': '02A',
  'Surf Tremolo': '02B',
  'Ambient Swell': '02C',
  'Rotary Vibe': '02D',
  'Tweed Edge': '03A',
  'Blues Drive': '03B',
  'Plexi Crunch': '03C',
  'Phase Rock': '03D',
  'Mandarin Crunch': '03E',
  'Wah Rock': '03F',
  'JCM Hot Rhythm': '04A',
  'Recto Rhythm': '04B',
  'Modern Tight': '04C',
  'Invader Chug': '04D',
  'Plexi Lead': '05A',
  'Singing Lead': '05B',
  'Hot Rod Lead': '05C',
  '80s Rack Lead': '05D',
  'Fuzz Lead': '05E',
  'Sustain Lead': '05F',
  'Phin Drive Echo': '06A',
  'Molam Swirl': '06B',
  'Khaen Wide': '06C',
  'Bass Foundation': '07A',
}

/** What a preset load leaves alone: the insert's power and input trim belong
 *  to the player's rig, not the tone; `clear_clip` is an action. */
const RIG_IDS = new Set(['power', 'input_trim', 'clear_clip'])

const close = (a: number, b: number) => Math.abs(a - b) <= 1e-4 * Math.max(1, Math.abs(b))

/** Whether the current values are preset `index` as loaded. */
function isPreset(editor: Editor, index: number): boolean {
  const preset = editor.spec.presets[index]
  if (!preset) return false
  const current = editor.values()
  return editor.spec.ids.every((id, i) => RIG_IDS.has(id) || close(current[i], preset.values[i] ?? 0))
}

function matchingPreset(editor: Editor): number | null {
  const i = editor.spec.presets.findIndex((_, index) => isPreset(editor, index))
  return i < 0 ? null : i
}

// ── The editor ──────────────────────────────────────────────────────────

function Rodhareist(props: { editor: Editor }) {
  const { editor } = props
  const [requested, setFocus] = useState<Focus | null>(null)
  // The preset last loaded, or matched on open; "edited" once the values
  // move away from it.
  const [preset, setPreset] = useState<number | null>(() => matchingPreset(editor))
  const order = readOrder(editor)
  const focus = resolveFocus(order, requested)

  const loadPreset = (index: number) => {
    const values = editor.spec.presets[index]?.values
    if (!values) return
    const changes: Record<string, number> = {}
    editor.spec.ids.forEach((id, i) => {
      if (!RIG_IDS.has(id) && values[i] !== undefined) changes[id] = values[i]
    })
    applyWire(editor, changes)
    setPreset(index)
    setFocus(null)
  }

  return (
    <div className="rh">
      <PresetBar editor={editor} preset={preset} />
      <div className="rh-main">
        <PresetBrowser editor={editor} active={preset} onLoad={loadPreset} />
        <div className="rh-work">
          <SignalPath editor={editor} order={order} focus={focus} onFocus={setFocus} />
          <EditPanel editor={editor} order={order} focus={focus} onFocus={setFocus} />
        </div>
      </div>
    </div>
  )
}

// ── Preset bar ──────────────────────────────────────────────────────────

function PowerButton(props: { on: boolean; color: string; title: string; onClick: () => void }) {
  return (
    <button
      type="button"
      className="rh-power"
      title={props.title}
      aria-label={props.title}
      aria-pressed={props.on}
      style={{ color: props.on ? props.color : 'var(--text-faint)' }}
      onClick={props.onClick}
    >
      <Power size={16} strokeWidth={2.25} />
    </button>
  )
}

function Trim(props: { editor: Editor; id: string; label: string }) {
  const { editor } = props
  const spec = describe(editor, props.id, props.id, props.label)
  if (!spec) return null
  const value = editor.value(spec.id)
  return (
    <div className="rh-trim">
      <span className="rh-caption">{props.label}</span>
      <div className="rh-trim-slider">
        <Slider editor={editor} spec={spec} accent={AMBER} />
      </div>
      <span className="rh-trim-value">{formatValue(value, spec)}</span>
    </div>
  )
}

function PresetBar(props: { editor: Editor; preset: number | null }) {
  const { editor, preset } = props
  const bank = editor.spec.presets
  const loaded = preset !== null ? bank[preset] : undefined
  const edited = preset !== null && loaded !== undefined && !isPreset(editor, preset)
  const power = editor.flag('power')
  return (
    <div className="rh-bar">
      <div className="rh-preset-name">
        <span className={loaded ? '' : 'none'}>{loaded ? loaded.name : 'No preset'}</span>
        {edited && <span className="rh-caption">EDITED</span>}
      </div>
      <span className="rh-spacer" />
      <div className="rh-bar-group">
        <Trim editor={editor} id="input_trim" label="IN" />
        <Trim editor={editor} id="output_trim" label="OUT" />
      </div>
      <div className="rh-bar-group">
        <span className="rh-caption">TEMPO</span>
        {/* The plug-in follows the host transport's tempo; LiveStage runs
            no transport, so there is none to show (the native "—"). */}
        <div className="rh-tempo" title="Host tempo — LiveStage has no transport tempo">
          —
        </div>
        <PowerButton
          on={power}
          color={AMBER}
          title={power ? 'Rodhareist on — click to bypass' : 'Rodhareist bypassed — click to turn on'}
          onClick={() => editor.set('power', power ? 0 : 1)}
        />
      </div>
    </div>
  )
}

// ── Preset browser ──────────────────────────────────────────────────────

function PresetBrowser(props: { editor: Editor; active: number | null; onLoad: (index: number) => void }) {
  const bank = props.editor.spec.presets
  return (
    <section className="rh-presets">
      <div className="rh-presets-head">
        <strong>PRESETS</strong>
        <span className="rh-caption">{bank.length}</span>
      </div>
      <div className="rh-presets-list">
        {bank.map((preset, index) => (
          <button
            key={preset.name}
            type="button"
            className={`rh-preset${props.active === index ? ' on' : ''}`}
            onClick={() => props.onLoad(index)}
          >
            <span className="rh-preset-id">{PRESET_IDS[preset.name] ?? ''}</span>
            <span className="rh-preset-title">{preset.name}</span>
          </button>
        ))}
      </div>
    </section>
  )
}

// ── Signal path ─────────────────────────────────────────────────────────

const METER_MARKS = ['0', '-12', '-24', '-36', '-48']

/** -48..0 dBFS onto 0..1, the scale the labels print. */
function meterFraction(level: number): number {
  if (level <= 1e-6) return 0
  return Math.min(1, Math.max(0, (20 * Math.log10(level) + 48) / 48))
}

/** One meter column: caption, the level bar beside its scale, and the
 *  click-to-clear clip latch. */
function MeterColumn(props: { editor: Editor; label: string; side: 'in' | 'out' }) {
  const { editor, side } = props
  const clip = useRef<HTMLButtonElement>(null)
  const scaleLeft = side === 'in'

  const draw = (ctx: Ctx, w: number, h: number) => {
    const c = colors()
    // No reading (stopped, bypassed): the native meter's zeroed frame.
    const f = editor.live.frame
    const level = (v: number | undefined) => Math.min(2, Math.max(0, v ?? 0))
    const rms = level(side === 'in' ? f?.in_rms : f?.out_rms)
    const peak = level(side === 'in' ? f?.in_peak : f?.out_peak)
    const clipped = !!f && (side === 'in' ? f.in_clip : f.out_clip)

    const SCALE_W = 16
    const BAR_W = 8
    const x0 = Math.round((w - (SCALE_W + 4 + BAR_W)) / 2)
    const scaleX = scaleLeft ? x0 : x0 + BAR_W + 4
    const barX = scaleLeft ? x0 + SCALE_W + 4 : x0
    const LINE = 11
    METER_MARKS.forEach((mark, i) => {
      paintLabel(ctx, mark, 9, c.textMuted, scaleX, Math.round((i * (h - LINE)) / 4))
    })
    rect(ctx, barX, 0, BAR_W, h, c.panel)
    // RMS fill, coloured by how hot it runs; the peak as a hairline.
    const fill = meterFraction(rms)
    const color = fill > 0.9 ? c.meterHigh : fill > 0.72 ? c.meterMid : c.meterLow
    if (fill > 0) rect(ctx, barX, h - fill * h, BAR_W, fill * h, color)
    const top = meterFraction(peak)
    if (top > 0) rect(ctx, barX, Math.max(0, h - top * h - 2), BAR_W, 2, c.text)

    const badge = clip.current
    if (badge && badge.classList.contains('on') !== clipped) {
      badge.classList.toggle('on', clipped)
      badge.title = clipped ? 'Clipped — click to clear' : 'Clip indicator'
    }
  }

  return (
    <div className="rh-meter">
      <span className="rh-caption">{props.label}</span>
      <LiveCanvas className="rh-meter-canvas" draw={draw} />
      <button
        ref={clip}
        type="button"
        className="rh-clip"
        title="Clip indicator"
        // One action clears both latches, as the native badges do.
        onClick={() => editor.set('clear_clip', 1)}
      >
        CLIP
      </button>
    </div>
  )
}

function PathNode(props: { editor: Editor; slot: number; kind: number; selected: boolean; onClick: () => void }) {
  const { editor, slot, kind, selected } = props
  const category = categoryOf(kind)
  const accent = TINT[category.tint]
  const on = isOn(editor, kind)
  const name = STAGES[kind].name
  return (
    <div className="rh-node-cell">
      <button
        type="button"
        className={`rh-node${selected ? ' selected' : ''}${on ? ' on' : ''}`}
        style={{ '--rh-accent': accent } as CSSProperties}
        title={`Slot ${slot + 1} · ${name}: ${modelName(editor, kind) ?? 'No amp'}${on ? '' : ' (bypassed)'}`}
        aria-pressed={selected}
        onClick={props.onClick}
      >
        {category.glyph}
      </button>
      <span className={`rh-node-label${on ? '' : ' off'}`}>{name}</span>
    </div>
  )
}

function SignalPath(props: {
  editor: Editor
  order: (number | null)[]
  focus: Focus
  onFocus: (focus: Focus) => void
}) {
  const { editor, order, focus, onFocus } = props
  const lane: ReactNode[] = []
  order.forEach((kind, slot) => {
    lane.push(
      <span key={`w${slot}`} className="rh-wire-cell">
        <span className="rh-wire" />
      </span>,
    )
    lane.push(
      kind !== null ? (
        <PathNode
          key={`n${slot}`}
          editor={editor}
          slot={slot}
          kind={kind}
          selected={sameFocus(focus, { block: kind })}
          onClick={() => onFocus({ block: kind })}
        />
      ) : (
        <button
          key={`s${slot}`}
          type="button"
          className={`rh-socket${sameFocus(focus, { empty: slot }) ? ' selected' : ''}`}
          title={`Slot ${slot + 1} · empty — click to add a block`}
          aria-label={`Slot ${slot + 1}, empty`}
          onClick={() => onFocus({ empty: slot })}
        >
          <span />
        </button>
      ),
    )
  })
  const placed = order.filter((kind) => kind !== null).length
  return (
    <section className="rh-path">
      <MeterColumn editor={editor} label="INPUT" side="in" />
      <div className="rh-routing">
        <div className="rh-routing-head">
          <span className="rh-caption">PATH</span>
          <span className="rh-caption">
            {placed} of {PATH_SLOTS} slots
          </span>
        </div>
        <div className="rh-lane">
          <div className="rh-lane-inner">
            <span className="rh-port-cell">
              <span className="rh-port">IN</span>
            </span>
            {lane}
            <span className="rh-wire-cell tail">
              <span className="rh-wire" />
            </span>
            <span className="rh-port-cell">
              <span className="rh-port">OUT</span>
            </span>
          </div>
        </div>
      </div>
      <MeterColumn editor={editor} label="MAIN" side="out" />
    </section>
  )
}

// ── Edit panel ──────────────────────────────────────────────────────────

interface PanelProps {
  editor: Editor
  order: (number | null)[]
  focus: Focus
  onFocus: (focus: Focus) => void
}

function EditPanel(props: PanelProps) {
  return (
    <section className="rh-edit">
      <div className="rh-edit-head">EDIT</div>
      <div className="rh-edit-cols">
        <CategoryColumn {...props} />
        <ModelColumn {...props} />
        <ParamColumn {...props} />
      </div>
    </section>
  )
}

function CategoryColumn(props: PanelProps) {
  const { editor, order, focus, onFocus } = props
  const slot = focusSlot(order, focus)
  const current = 'block' in focus ? categoryOf(focus.block) : null
  return (
    <div className="rh-categories">
      {CATEGORIES.map((category) => {
        const active = current === category
        const accent = TINT[category.tint]
        const target = stageFor(order, slot, category)
        const enabled = active || target !== null
        const title = active
          ? undefined
          : target !== null
            ? 'empty' in focus
              ? `Add ${STAGES[target].name} to slot ${slot + 1}`
              : `Replace this block with ${STAGES[target].name}`
            : `Every ${category.name} block is already on the path`
        return (
          <button
            key={category.name}
            type="button"
            className={`rh-category${active ? ' on' : ''}${enabled ? '' : ' full'}`}
            style={{ '--rh-accent': accent } as CSSProperties}
            title={title}
            aria-pressed={active}
            onClick={() => {
              if (active || target === null) return
              pathEdit(editor, [[slot, target]])
              onFocus({ block: target })
            }}
          >
            <span className="rh-dot" />
            <span>{category.name}</span>
          </button>
        )
      })}
    </div>
  )
}

function ModelColumn(props: PanelProps) {
  const { editor, focus } = props
  if (!('block' in focus)) {
    return (
      <div className="rh-models">
        <p className="rh-models-hint">Pick a category to add a block to this slot.</p>
      </div>
    )
  }
  const kind = focus.block
  const accent = TINT[categoryOf(kind).tint]
  const [list, selected] = models(editor, kind)
  return (
    <div className="rh-models" style={{ '--rh-accent': accent } as CSSProperties}>
      {list.map((model, index) => {
        const active = selected === index
        return (
          <button
            key={model.label}
            type="button"
            className={`rh-model${active ? ' on' : ''}`}
            aria-pressed={active}
            onClick={() => {
              if (model.wire && !active) setWire(editor, model.wire, model.value)
            }}
          >
            {model.label}
          </button>
        )
      })}
    </div>
  )
}

/** A horizontal slider as the native `slider_with_reset`: linear over
 *  `min..max`, a press does not jump (only a drag moves it), double-click
 *  resets to the plug-in's default. */
function Slider(props: { editor: Editor; spec: ParamSpec; accent: string }) {
  const { editor, spec } = props
  const box = useRef<HTMLDivElement>(null)
  const drag = useRef<{ x: number; moved: boolean } | null>(null)
  const span = Math.max(1e-9, spec.max - spec.min)
  const value = editor.value(spec.id)
  const pos = Math.min(1, Math.max(0, (value - spec.min) / span))

  const setAt = (clientX: number) => {
    const r = box.current!.getBoundingClientRect()
    const t = Math.min(1, Math.max(0, (clientX - r.left) / Math.max(1, r.width)))
    editor.set(spec.id, spec.min + t * span)
  }
  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return
    e.currentTarget.setPointerCapture(e.pointerId)
    drag.current = { x: e.clientX, moved: false }
  }
  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    const d = drag.current
    if (!d) return
    // A drag threshold, so a click (or the first half of a double-click)
    // never moves the value.
    if (!d.moved && Math.abs(e.clientX - d.x) < 2) return
    d.moved = true
    setAt(e.clientX)
  }
  const end = () => {
    drag.current = null
  }
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const step = { ArrowRight: 0.01, ArrowUp: 0.01, ArrowLeft: -0.01, ArrowDown: -0.01, PageUp: 0.1, PageDown: -0.1 }[
      e.key
    ]
    let next: number | null = null
    if (step !== undefined) next = Math.min(1, Math.max(0, pos + step))
    else if (e.key === 'Home') next = 0
    else if (e.key === 'End') next = 1
    if (next === null) return
    e.preventDefault()
    editor.set(spec.id, spec.min + next * span)
  }

  return (
    <div
      ref={box}
      className="rh-slider"
      role="slider"
      tabIndex={0}
      aria-label={`${spec.label} (double-click: default)`}
      aria-valuemin={spec.min}
      aria-valuemax={spec.max}
      aria-valuenow={value}
      aria-valuetext={formatValue(value, spec)}
      style={{ '--rh-accent': props.accent, '--rh-pos': pos } as CSSProperties}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={end}
      onPointerCancel={end}
      onDoubleClick={() => editor.set(spec.id, editor.defaultOf(spec.id))}
      onKeyDown={onKeyDown}
    >
      <span className="rh-slider-rail" />
      <span className="rh-slider-fill" />
      <span className="rh-slider-thumb" />
    </div>
  )
}

function ParamRow(props: { editor: Editor; spec: ParamSpec; accent: string }) {
  const { editor, spec } = props
  return (
    <div className={`rh-row${spec.inactive ? ' inactive' : ''}`}>
      <span className="rh-row-label">{spec.label}</span>
      <div className="rh-row-control">
        <Slider editor={editor} spec={spec} accent={props.accent} />
      </div>
      <span className="rh-row-value" style={{ color: props.accent }}>
        {formatValue(editor.value(spec.id), spec)}
      </span>
    </div>
  )
}

/** A row of choice chips (microphone type, capture loudness). */
function ChipRow(props: { editor: Editor; label: string; choices: Choice[]; selected: number | null; accent: string }) {
  return (
    <div className="rh-row">
      <span className="rh-row-label">{props.label}</span>
      <div className="rh-chips" style={{ '--rh-accent': props.accent } as CSSProperties}>
        {props.choices.map((choice, index) => {
          const active = props.selected === index
          return (
            <button
              key={choice.label}
              type="button"
              className={`rh-chip${active ? ' on' : ''}`}
              aria-pressed={active}
              onClick={() => !active && setWire(props.editor, choice.wire, choice.value)}
            >
              {choice.label}
            </button>
          )
        })}
      </div>
    </div>
  )
}

/** Where the native editor has a file loader (IR, NAM capture). */
function LoaderNote(props: { label: string; what: string }) {
  return (
    <div className="rh-row">
      <span className="rh-row-label">{props.label}</span>
      <span className="rh-note">
        {props.what} load from a file on the machine running the engine, which this page cannot open.
      </span>
    </div>
  )
}

function ParamColumn(props: PanelProps) {
  const { editor, order, focus, onFocus } = props
  if (!('block' in focus)) {
    return (
      <div className="rh-params">
        <div className="rh-params-head">
          <strong>Slot {focus.empty + 1} — empty</strong>
        </div>
      </div>
    )
  }
  const kind = focus.block
  const accent = TINT[categoryOf(kind).tint]
  const on = isOn(editor, kind)
  const slot = focusSlot(order, focus)
  const last = PATH_SLOTS - 1
  const nam = kind === Stage.Amp && toneEngine(editor) === ENGINE_NAM
  const cabIr = modelIndex(editor.value('cab_model'), CAB_MODELS.length) === CAB_IR
  const swap = (other: number) => {
    pathEdit(editor, [
      [slot, order[other]],
      [other, order[slot]],
    ])
    onFocus({ block: kind })
  }
  return (
    <div className="rh-params">
      <div className="rh-params-head">
        <strong>
          {STAGES[kind].name} — {modelName(editor, kind) ?? 'No amp'}
        </strong>
        <button
          type="button"
          className="rh-action"
          title="Move block earlier in the path"
          aria-label="Move block earlier in the path"
          disabled={slot <= 0}
          onClick={() => swap(slot - 1)}
        >
          <ChevronLeft size={14} />
        </button>
        <button
          type="button"
          className="rh-action"
          title="Move block later in the path"
          aria-label="Move block later in the path"
          disabled={slot >= last}
          onClick={() => swap(slot + 1)}
        >
          <ChevronRight size={14} />
        </button>
        <button
          type="button"
          className="rh-action"
          title="Remove block from the path"
          aria-label="Remove block from the path"
          onClick={() => {
            pathEdit(editor, [[slot, null]])
            onFocus({ empty: slot })
          }}
        >
          <X size={14} />
        </button>
        <span className="rh-caption">{on ? 'ON' : 'BYPASSED'}</span>
        <PowerButton
          on={on}
          color={accent}
          title={on ? 'Bypass block' : 'Enable block'}
          onClick={() => editor.set(STAGES[kind].enable, on ? 0 : 1)}
        />
      </div>
      <div className={`rh-params-body${on ? '' : ' off'}`}>
        {kind === Stage.Cab && (
          <ChipRow
            editor={editor}
            label="Microphone"
            choices={MIC_MODELS.map((label, value) => ({ label, wire: 'cab_mic_type', value }))}
            selected={modelIndex(editor.value('cab_mic_type'), MIC_MODELS.length)}
            accent={accent}
          />
        )}
        {kind === Stage.Cab && cabIr && <LoaderNote label="Impulse" what="Impulse responses" />}
        {nam && <LoaderNote label="Capture" what="NAM captures" />}
        {params(editor, kind).map((spec) => (
          <ParamRow key={spec.id} editor={editor} spec={spec} accent={accent} />
        ))}
        {nam && (
          <ChipRow
            editor={editor}
            label="Loudness"
            choices={[
              { label: 'Raw', wire: 'nam_loudness_norm', value: 0 },
              { label: 'Normalized', wire: 'nam_loudness_norm', value: 1 },
            ]}
            selected={editor.flag('nam_loudness_norm') ? 1 : 0}
            accent={accent}
          />
        )}
      </div>
    </div>
  )
}

export const editors: Record<string, EditorComponent> = {
  rodharerist: Rodhareist,
}
