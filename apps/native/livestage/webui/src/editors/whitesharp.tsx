// WhiteSharp's editor: a port of the native white_sharp_panel.rs (layout),
// white_sharp_model.rs (knob table, preset style ids), white_sharp_window.rs
// (key/scale/note-list edits) and white_sharp_meter.rs (the correction meter
// and the keyboard's live lights), laid out like Auto-Tune Pro's Auto mode:
// voice and key settings, the pitch correction meter, the correction knobs
// round Retune Speed, the keyboard, Create Vibrato and Output.

import { useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { ChevronLeft, ChevronRight } from 'lucide-react'
import { Knob } from '../controls.tsx'
import type { Editor, EditorComponent } from './kit.tsx'
import { Card, Choice, EditorShell, KitKnob, KNOB, LiveCanvas, ParamCheck, Row } from './kit.tsx'
import type { KnobSpec } from './knobspec.ts'
import { bipolar, fromKnob, knobRange, readout, spec, toKnob } from './knobspec.ts'
import type { Ctx } from './paint.ts'
import { alpha, colors, rect } from './paint.ts'
import './whitesharp.css'

// ── The crate's tables (whitesharp/src/scale.rs, lib.rs) ────────────────

const NOTE_NAMES = ['C', 'C#', 'D', 'D#', 'E', 'F', 'F#', 'G', 'G#', 'A', 'A#', 'B']

/** Scale::ALL in wire order, with Scale::steps. */
const SCALES: readonly (readonly [string, readonly number[]])[] = [
  ['Chromatic', [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]],
  ['Major', [0, 2, 4, 5, 7, 9, 11]],
  ['Minor', [0, 2, 3, 5, 7, 8, 10]],
  ['Harmonic Minor', [0, 2, 3, 5, 7, 8, 11]],
  ['Melodic Minor', [0, 2, 3, 5, 7, 9, 11]],
  ['Major Pentatonic', [0, 2, 4, 7, 9]],
  ['Minor Pentatonic', [0, 3, 5, 7, 10]],
  ['Blues', [0, 3, 5, 6, 7, 10]],
  ['Dorian', [0, 2, 3, 5, 7, 9, 10]],
  ['Phrygian', [0, 1, 3, 5, 7, 8, 10]],
  ['Lydian', [0, 2, 4, 6, 7, 9, 11]],
  ['Mixolydian', [0, 2, 4, 5, 7, 9, 10]],
]

/** InputType::ALL in wire order. */
const INPUT_TYPES = ['Soprano', 'Alto/Tenor', 'Low Male', 'Instrument', 'Bass Inst.']

/** VibratoShape::ALL in wire order. */
const VIBRATO_SHAPES: readonly (readonly [number, string])[] = [
  [0, 'Off'],
  [1, 'Sine'],
  [2, 'Square'],
  [3, 'Saw'],
]

const ALL_NOTES = 0x0fff

/** The pitch classes of scale `scale` in `key` (scale_mask). */
function scaleMask(key: number, scale: number): number {
  const steps = SCALES[Math.min(SCALES.length - 1, Math.max(0, scale))][1]
  return steps.reduce((mask, step) => mask | (1 << ((key + step) % 12)), 0)
}

/** `note` (MIDI) as a name and octave, `A4` for 69 (note_name). */
function noteName(note: number): string {
  const cls = ((note % 12) + 12) % 12
  return `${NOTE_NAMES[cls]}${Math.floor(note / 12) - 1}`
}

// ── Knobs (white_sharp_model::knob) ─────────────────────────────────────

const KNOBS: Record<string, KnobSpec> = {
  // Fine at the fast end, where the character changes most.
  retuneMs: spec('retuneMs', 'Retune Speed', 0, 400, 'square', 'ms'),
  humanize: spec('humanize', 'Humanize', 0, 100, 'linear', 'percent'),
  flexTune: spec('flexTune', 'Flex-Tune', 0, 100, 'linear', 'percent'),
  vibratoDb: bipolar(spec('vibratoDb', 'Vibrato', -12, 12, 'linear', 'db'), 0),
  throat: bipolar(spec('throat', 'Throat', 70, 140, 'linear', 'percent'), 100),
  transpose: bipolar(spec('transpose', 'Transpose', -24, 24, 'linear', 'semitones'), 0),
  detune: bipolar(spec('detune', 'Detune', -100, 100, 'linear', 'cents'), 0),
  tracking: spec('tracking', 'Tracking', 0, 100, 'linear', 'percent'),
  mix: spec('mix', 'Mix', 0, 100, 'linear', 'percent'),
  vibratoRateHz: spec('vibratoRateHz', 'Rate', 0.1, 10, 'log', 'hz'),
  vibratoDelayMs: spec('vibratoDelayMs', 'Delay', 0, 2000, 'square', 'ms'),
  vibratoOnsetMs: spec('vibratoOnsetMs', 'Onset', 0, 2000, 'square', 'ms'),
  vibratoPitch: spec('vibratoPitch', 'Pitch', 0, 100, 'linear', 'centsDepth'),
  vibratoAmp: spec('vibratoAmp', 'Amplitude', 0, 100, 'linear', 'percent'),
  vibratoVariation: spec('vibratoVariation', 'Variation', 0, 100, 'linear', 'percent'),
  outputDb: bipolar(spec('outputDb', 'Output', -24, 12, 'linear', 'db'), 0),
}

/** What a preset sets is the correction *style* (STYLE_IDS); the key, the
 *  scale, the note lists, the input type, tuning and levels belong to the
 *  song and the singer, so a preset leaves them as they are. */
const SONG_IDS = ['key', 'scale', 'inputType', 'detune', 'tracking', 'removeMask', 'bypassMask', 'mix', 'outputDb']

const SIDE_KNOB = 52
/** Retune Speed is the control the whole plug-in turns on. */
const HERO = 84

// ── The live displays (white_sharp_meter.rs) ────────────────────────────

/** The meter's reach either way, in cents. */
const METER_CENTS = 100
/** How fast the needle follows / falls back to centre, per 60 Hz frame. */
const FOLLOW = 0.35
const RELEASE = 0.12
/** Readings a pause in the voice may last before the lights go out. */
const RECENT = 6
/** telemetry::SLOTS and POINTS. */
const SLOTS = 64
const POINTS = Math.floor((SLOTS - 1) / 3)
/** A block older than this means the insert stopped publishing: no signal. */
const STALE_MS = 500

interface Reading {
  input: number | null
  output: number | null
  target: number | null
}

/** The newest sung reading in a block, if the voice is sounding
 *  (telemetry::decode, then newest_voiced). */
function newestVoiced(slots: number[]): Reading | null {
  if (slots.length < SLOTS) return null
  const pitch = (v: number) => (Number.isFinite(v) && v >= 0 ? v : null)
  for (let i = POINTS - 1; i >= POINTS - RECENT; i--) {
    const base = 1 + 3 * i
    const input = pitch(slots[base])
    if (input === null) continue
    const target = pitch(slots[base + 2])
    return { input, output: pitch(slots[base + 1]), target: target === null ? null : Math.round(target) }
  }
  return null
}

/** What the meter and the key lights carry across frames. */
interface LiveState {
  seenAt: number
  reading: Reading | null
  needle: number
  lastFrame: number
}

/** Takes the newest block, if new, and moves the needle a frame's worth
 *  (LiveDisplay::poll). Runs from both canvases; the frame time keeps the
 *  second call in a frame from moving the needle twice. */
function poll(editor: Editor, state: LiveState, now: number) {
  const { live } = editor
  if (live.pitch && live.pitchAt !== state.seenAt) {
    state.seenAt = live.pitchAt
    state.reading = newestVoiced(live.pitch)
  }
  if (now - live.pitchAt > STALE_MS) state.reading = null
  if (now === state.lastFrame) return
  const frames = state.lastFrame ? Math.min(10, (now - state.lastFrame) / (1000 / 60)) : 1
  state.lastFrame = now
  const r = state.reading
  const wanted =
    r && r.input !== null && r.output !== null
      ? Math.min(METER_CENTS, Math.max(-METER_CENTS, (r.output - r.input) * 100))
      : null
  // The native rates are per frame at 60 Hz; scaled so a slower or faster
  // screen follows in the same time.
  const k = 1 - Math.pow(1 - (wanted === null ? RELEASE : FOLLOW), frames)
  state.needle += ((wanted ?? 0) - state.needle) * k
}

/** What is sung against what it is pulled to (LiveDisplay::readout). */
function readoutOf(reading: Reading | null): string {
  if (!reading || reading.input === null) return ''
  const nearest = Math.round(reading.input)
  const c = Math.round((reading.input - nearest) * 100)
  const cents = `${c >= 0 ? '+' : '-'}${Math.abs(c)}¢`
  return reading.target === null
    ? `${noteName(nearest)} ${cents}`
    : `${noteName(nearest)} ${cents}  →  ${noteName(reading.target)}`
}

/** The meter: ticks every ten cents, a bar from the centre to the
 *  correction and the needle at its end. Left of centre the voice is pulled
 *  down, right of it up (meter_scene). */
function paintMeter(ctx: Ctx, w: number, h: number, needle: number) {
  if (w < 8 || h < 8) return
  const c = colors()
  const ink = c.text
  const inset = 10
  const left = inset
  const span = w - 2 * inset
  const mid = left + span * 0.5
  const x = (cents: number) => mid + (cents / METER_CENTS) * span * 0.5
  const channelY = h * 0.42
  const channelH = h * 0.3
  rect(ctx, left, channelY, span, channelH, alpha(ink, 0.06))
  for (let cents = -METER_CENTS; cents <= METER_CENTS + 0.5; cents += 10) {
    const centre = Math.abs(cents) < 0.5
    const major = Math.abs(cents % 50) < 0.5
    const [top, a] = centre ? [h * 0.1, 0.55] : major ? [h * 0.2, 0.32] : [h * 0.3, 0.16]
    rect(ctx, x(cents) - 0.5, top, 1, channelY - top - 2, alpha(ink, a))
  }
  const end = x(needle)
  rect(ctx, Math.min(end, mid), channelY, Math.abs(end - mid), channelH, alpha(c.accent, 0.55))
  rect(ctx, end - 1, channelY - 4, 2, channelH + 8, c.accent)
  rect(ctx, mid - 0.5, channelY, 1, channelH, alpha(ink, 0.6))
}

/** Where pitch class `cls` sits on a one-octave keyboard: left edge and
 *  width as fractions of its width, and whether it is a black key. */
function keyFrame(cls: number): [number, number, boolean] {
  const WHITE = 1 / 7
  const BLACK = WHITE * 0.62
  switch (cls % 12) {
    case 0:
      return [0, WHITE, false]
    case 2:
      return [WHITE, WHITE, false]
    case 4:
      return [2 * WHITE, WHITE, false]
    case 5:
      return [3 * WHITE, WHITE, false]
    case 7:
      return [4 * WHITE, WHITE, false]
    case 9:
      return [5 * WHITE, WHITE, false]
    case 11:
      return [6 * WHITE, WHITE, false]
    case 1:
      return [WHITE - BLACK * 0.5, BLACK, true]
    case 3:
      return [2 * WHITE - BLACK * 0.5, BLACK, true]
    case 6:
      return [4 * WHITE - BLACK * 0.5, BLACK, true]
    case 8:
      return [5 * WHITE - BLACK * 0.5, BLACK, true]
    default:
      return [6 * WHITE - BLACK * 0.5, BLACK, true]
  }
}

/** A black key's height, as a share of the keyboard's. */
const BLACK_KEY_HEIGHT = 0.6

/** The note the voice is pulled to washed in the accent, with a bar; the
 *  sung note, where it differs, ringed (LiveDisplay::paint_keys). */
function paintKeys(ctx: Ctx, w: number, h: number, reading: Reading | null) {
  if (!reading) return
  const accent = colors().accent
  const keyRect = (cls: number): [number, number, number, number] => {
    const [left, width, black] = keyFrame(cls)
    return [left * w, 0, width * w, black ? h * BLACK_KEY_HEIGHT : h]
  }
  const target = reading.target === null ? null : ((reading.target % 12) + 12) % 12
  if (target !== null) {
    const [x, y, kw, kh] = keyRect(target)
    // A white key shows only below the black keys; washing all of it would
    // tint its neighbours.
    const top = keyFrame(target)[2] ? y : y + h * BLACK_KEY_HEIGHT
    rect(ctx, x, top, kw, y + kh - top, alpha(accent, 0.45))
    rect(ctx, x + 3, y + kh - 6, Math.max(2, kw - 6), 3, accent)
  }
  if (reading.input !== null) {
    const cls = ((Math.round(reading.input) % 12) + 12) % 12
    if (cls !== target) {
      const [x, y, kw, kh] = keyRect(cls)
      ctx.strokeStyle = accent
      ctx.lineWidth = 2
      ctx.strokeRect(x + 1, y + 1, kw - 2, kh - 2)
    }
  }
}

// ── Pieces ──────────────────────────────────────────────────────────────

/** A knob with its name and value under it. Greyed, with `why` for its
 *  value and no response, when it has nothing to do (knob_for / big_knob). */
function WsKnob(props: { editor: Editor; id: string; size?: number; why?: string | null; big?: boolean }) {
  const { editor } = props
  const s = KNOBS[props.id]
  const size = props.size ?? KNOB
  const className = `pe-knob${props.big ? ' ws-big' : ''}${size >= HERO ? ' ws-hero' : ''}`
  if (!props.why && !props.big) return <KitKnob editor={editor} spec={s} size={size} />
  const [lo, hi] = knobRange(s)
  const current = toKnob(s, editor.value(s.id))
  if (props.why) {
    const why = props.why
    return (
      <div className={`${className} ws-idle`} style={props.big ? { width: size + 48 } : undefined}>
        <Knob
          value={current}
          min={lo}
          max={hi}
          defaultValue={current}
          bipolar={s.bipolar}
          size={size}
          label={s.label}
          caption={s.label}
          format={() => why}
          onChange={() => {}}
        />
      </div>
    )
  }
  return (
    <div className={className} style={{ width: size + 48 }}>
      <Knob
        value={current}
        min={lo}
        max={hi}
        defaultValue={toKnob(s, editor.defaultOf(s.id))}
        bipolar={s.bipolar}
        size={size}
        label={`${s.label} (double-click: default)`}
        caption={s.label}
        format={(units) => readout(s, fromKnob(s, units))}
        onChange={(units) => editor.set(s.id, fromKnob(s, units))}
      />
    </div>
  )
}

/** A labelled setting: its caption over its control. */
function Setting(props: { title: string; children: ReactNode }) {
  return (
    <div className="ws-setting">
      <span className="ws-caption">{props.title}</span>
      {props.children}
    </div>
  )
}

function Picker(props: { value: number; options: readonly string[]; onChange: (v: number) => void; width: number; title: string }) {
  return (
    <label className="select ws-picker" style={{ width: props.width }} title={props.title}>
      <select value={props.value} onChange={(e) => props.onChange(Number(e.currentTarget.value))}>
        {props.options.map((text, i) => (
          <option key={text} value={i}>
            {text}
          </option>
        ))}
      </select>
      <svg className="select-chevron" width="10" height="10" viewBox="0 0 10 10" aria-hidden>
        <path d="M2 3.5 5 6.5 8 3.5" />
      </svg>
    </label>
  )
}

// ── The editor ──────────────────────────────────────────────────────────

type NoteList = 'remove' | 'bypass'

function WhiteSharp(props: { editor: Editor }) {
  const { editor } = props
  const [keyEdit, setKeyEdit] = useState<NoteList>('remove')
  const state = useRef<LiveState>({ seenAt: 0, reading: null, needle: 0, lastFrame: 0 })
  const readoutSpan = useRef<HTMLSpanElement>(null)
  const shownReadout = useRef('')

  const key = Math.min(11, Math.max(0, Math.round(editor.value('key'))))
  const scale = Math.min(SCALES.length - 1, Math.max(0, Math.round(editor.value('scale'))))
  const inputType = Math.min(INPUT_TYPES.length - 1, Math.max(0, Math.round(editor.value('inputType'))))
  const removeMask = Math.round(editor.value('removeMask')) & ALL_NOTES
  const bypassMask = Math.round(editor.value('bypassMask')) & ALL_NOTES
  const classic = editor.flag('classic')
  const shape = Math.round(editor.value('vibratoShape'))

  // A new key or scale starts from its own notes: lists made for the old
  // one would mark the wrong ones.
  const setKey = (k: number) => editor.setMany({ key: ((k % 12) + 12) % 12, removeMask: 0, bypassMask: 0 })
  const setScale = (s: number) => editor.setMany({ scale: s, removeMask: 0, bypassMask: 0 })

  // A note is on one list at a time: removing a bypassed note un-bypasses
  // it, and so on.
  const toggleNote = (cls: number) => {
    const bit = 1 << cls
    let remove = removeMask
    let bypass = bypassMask
    if (keyEdit === 'remove') {
      remove ^= bit
      bypass &= ~(remove & bit)
    } else {
      bypass ^= bit
      remove &= ~(bypass & bit)
    }
    editor.setMany({ removeMask: remove, bypassMask: bypass })
  }

  const inScale = scaleMask(key, scale)
  // White keys first, black keys over them.
  const order = Array.from({ length: 12 }, (_, i) => i).sort((a, b) => Number(keyFrame(a)[2]) - Number(keyFrame(b)[2]))
  const modernOnly = classic ? 'Classic' : null
  const vibratoOff = shape === 0 ? 'off' : null

  return (
    <EditorShell editor={editor} title="WhiteSharp" subtitle="Auto pitch correction" keep={SONG_IDS} className="ws">
      <Card className="ws-settings">
        <div className="ws-settings-row">
          <Setting title="INPUT TYPE">
            <Picker
              value={inputType}
              options={INPUT_TYPES}
              width={128}
              title="Input type"
              onChange={(v) => editor.set('inputType', v)}
            />
          </Setting>
          <Setting title="KEY">
            <div className="ws-cluster">
              <button type="button" className="icon-button" title="Key down" onClick={() => setKey(key + 11)}>
                <ChevronLeft size={14} />
              </button>
              <Picker value={key} options={NOTE_NAMES} width={64} title="Key" onChange={setKey} />
              <button type="button" className="icon-button" title="Key up" onClick={() => setKey(key + 1)}>
                <ChevronRight size={14} />
              </button>
            </div>
          </Setting>
          <Setting title="SCALE">
            <Picker value={scale} options={SCALES.map(([name]) => name)} width={150} title="Scale" onChange={setScale} />
          </Setting>
          <span className="ws-fill" />
          <Setting title="FORMANT">
            <div className="ws-formant">
              <ParamCheck editor={editor} id="formant" label="Keep formants" />
            </div>
          </Setting>
          <div className="ws-knobs">
            <WsKnob editor={editor} id="throat" />
            <WsKnob editor={editor} id="transpose" />
            <WsKnob editor={editor} id="detune" />
            <WsKnob editor={editor} id="tracking" />
          </div>
        </div>
      </Card>

      <Card
        title="Pitch correction"
        className="ws-meter-card"
        aside={<span ref={readoutSpan} className="ws-readout" />}
      >
        <LiveCanvas
          className="ws-meter"
          draw={(ctx, w, h, now) => {
            const s = state.current
            poll(editor, s, now)
            paintMeter(ctx, w, h, s.needle)
            // The readout changes a few times a second; written straight
            // to its span so a frame never re-renders the controls.
            const text = readoutOf(s.reading)
            if (text !== shownReadout.current && readoutSpan.current) {
              shownReadout.current = text
              readoutSpan.current.textContent = text
            }
          }}
        />
        <div className="ws-scale">
          {[-100, -50, 0, 50, 100].map((cents) => (
            <span key={cents} style={{ left: `${50 + (cents / METER_CENTS) * 50}%` }}>
              {cents > 0 ? `+${cents}` : `${cents}`}
            </span>
          ))}
        </div>
      </Card>

      <Card className="ws-correction">
        <div className="ws-correction-row">
          <WsKnob editor={editor} id="humanize" size={SIDE_KNOB} big why={modernOnly} />
          <WsKnob editor={editor} id="retuneMs" size={HERO} big />
          <div className="ws-flex">
            <WsKnob editor={editor} id="flexTune" size={SIDE_KNOB} big why={modernOnly} />
            <ParamCheck editor={editor} id="classic" label="Classic" />
          </div>
          <WsKnob editor={editor} id="vibratoDb" size={SIDE_KNOB} big />
        </div>
      </Card>

      <Card
        title={
          <span className="ws-keyhead">
            Keyboard edit
            <Choice
              value={keyEdit}
              options={[
                ['remove', 'Remove'],
                ['bypass', 'Bypass'],
              ]}
              onChange={setKeyEdit}
            />
          </span>
        }
        aside={
          <button
            type="button"
            className="button ghost"
            disabled={removeMask === 0 && bypassMask === 0}
            onClick={() => editor.setMany({ removeMask: 0, bypassMask: 0 })}
          >
            Clear
          </button>
        }
      >
        <div className="ws-keys">
          {order.map((cls) => {
            const [left, width, black] = keyFrame(cls)
            const bit = 1 << cls
            const scaled = (inScale & bit) !== 0
            const removed = (removeMask & bit) !== 0
            const bypassed = (bypassMask & bit) !== 0
            // A piano's colours: the scale's notes bright, the others
            // dimmed, a removed note dark, a bypassed one in the warning hue.
            const fill =
              bypassed && !removed
                ? 'bypassed'
                : removed
                  ? 'removed'
                  : scaled
                    ? 'in'
                    : 'out'
            const status = removed ? 'off' : bypassed ? 'bypass' : null
            const light = !black && !removed && (scaled || bypassed)
            return (
              <button
                key={cls}
                type="button"
                className={`ws-key ${black ? 'black' : 'white'} ${fill}${light ? ' light' : ''}`}
                style={{ left: `${left * 100}%`, width: `${width * 100}%`, height: `${(black ? BLACK_KEY_HEIGHT : 1) * 100}%` }}
                title={`${NOTE_NAMES[cls]}: ${keyEdit === 'remove' ? 'remove' : 'bypass'}`}
                onClick={() => toggleNote(cls)}
              >
                {NOTE_NAMES[cls]}
                {status && <small>{status}</small>}
              </button>
            )
          })}
          <LiveCanvas
            className="ws-lights"
            draw={(ctx, w, h, now) => {
              const s = state.current
              poll(editor, s, now)
              paintKeys(ctx, w, h, s.reading)
            }}
          />
        </div>
      </Card>

      <Row>
        <Card
          title="Create vibrato"
          grow={6}
          aside={
            <Choice value={shape} options={VIBRATO_SHAPES} onChange={(v) => editor.set('vibratoShape', v)} />
          }
        >
          <div className="pe-knobs">
            {['vibratoRateHz', 'vibratoDelayMs', 'vibratoOnsetMs', 'vibratoPitch', 'vibratoAmp', 'vibratoVariation'].map(
              (id) => (
                <WsKnob key={id} editor={editor} id={id} why={vibratoOff} />
              ),
            )}
          </div>
        </Card>
        <Card title="Output" grow={2} minWidth={150}>
          <div className="pe-knobs">
            <WsKnob editor={editor} id="mix" />
            <WsKnob editor={editor} id="outputDb" />
          </div>
        </Card>
      </Row>
    </EditorShell>
  )
}

export const editors: Record<string, EditorComponent> = {
  whitesharp: WhiteSharp,
}
