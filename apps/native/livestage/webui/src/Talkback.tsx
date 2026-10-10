// Talkback and the oscillator (Phase 3): the top bar's key, and the panel
// it opens beside the mixer (not over it: TALK is pressed while mixing).
//
// TALK: press and hold to talk (momentary); double-click or a long press
// latches it, and a press releases it. Touch, mouse and keyboard alike.
// The oscillator is one deliberate click, on in the warning hue with its
// level shown.

import { useEffect, useRef, useState } from 'react'
import type { KeyboardEvent as ReactKeyboardEvent, PointerEvent as ReactPointerEvent } from 'react'
import { AudioWaveform, Lock, Megaphone, TriangleAlert, X } from 'lucide-react'
import { Knob, LevelBar, Select } from './controls.tsx'
import { Choice } from './editors/kit.tsx'
import { dbToPosition, formatDb, positionToDb } from './faderLaw.ts'
import { OSC_HZ, OSC_LEVEL_DB, formatHz } from './processing.ts'
import type { Destination, Oscillator, OscillatorKind, Session, Talkback } from './protocol.ts'
import { TALKBACK_KEY, sameStrip, stripKey } from './protocol.ts'
import { destinations, liveDestinations, toggleDestination } from './routing.ts'
import { act, actLatest, useStore } from './store.ts'
import './talkback.css'

/** A press this long latches TALK. */
const LONG_PRESS_MS = 600
/** A second press this soon after the first latches TALK. */
const DOUBLE_MS = 350
/** How long a fresh latch outranks a status that has not caught up. */
const LATCH_GRACE_MS = 1500

/** What is live now, as the server's status says. */
export function useLiveTalk(): { talking: boolean; oscillator: boolean } {
  const talking = useStore((s) => s.status?.status.talkback_active === true)
  const oscillator = useStore((s) => s.status?.status.oscillator_on === true)
  return { talking, oscillator }
}

/** The top bar's key: lit while talkback is open, the oscillator in the
 *  warning hue while it sounds. Opens the panel. */
export function TalkbackButton(props: { open: boolean; onToggle: () => void }) {
  const { talking, oscillator } = useLiveTalk()
  return (
    <button
      type="button"
      className={`tb-key${talking ? ' talking' : ''}${oscillator ? ' osc' : ''}${props.open ? ' open' : ''}`}
      aria-expanded={props.open}
      title={`Talkback and oscillator${talking ? ' · talkback is open' : ''}${oscillator ? ' · the oscillator is ON' : ''}`}
      onClick={props.onToggle}
    >
      <Megaphone size={15} />
      <span className="tb-key-label">{talking ? 'TALK' : 'Talk'}</span>
      {oscillator && (
        <span className="tb-key-osc">
          <AudioWaveform size={12} /> OSC
        </span>
      )}
    </button>
  )
}

export function TalkbackPanel(props: { session: Session; inputs: number; onClose: () => void }) {
  const { session } = props
  const phase3 = useStore((s) => s.phase3)
  const old = phase3 === false
  return (
    <aside className="tb-panel" role="dialog" aria-label="Talkback and oscillator">
      <header className="tb-head">
        <Megaphone size={16} />
        <strong>Talkback &amp; oscillator</strong>
        <span className="spacer" />
        <button type="button" className="icon-button large" onClick={props.onClose} aria-label="Close" title="Close (Esc)">
          <X size={16} />
        </button>
      </header>
      {old && (
        <p className="tb-old">
          <TriangleAlert size={14} /> This LiveStage server predates talkback and the oscillator: its controls are
          refused. Update the server.
        </p>
      )}
      <TalkbackSection session={session} inputs={props.inputs} />
      <OscillatorSection session={session} />
    </aside>
  )
}

// ── Talkback ────────────────────────────────────────────────────────────

function TalkbackSection(props: { session: Session; inputs: number }) {
  const { session } = props
  const talkback = session.talkback
  const to = liveDestinations(session, talkback.to)
  const set = (patch: Partial<Talkback>) => act({ cmd: 'set_talkback', talkback: { ...talkback, ...patch } })
  const missing: string[] = []
  if (talkback.input === null) missing.push('no input chosen')
  else if (talkback.input >= props.inputs && props.inputs > 0) missing.push(`In ${talkback.input + 1} is not on this interface`)
  if (to.length === 0) missing.push('no destination ticked')
  return (
    <section className="tb-section">
      <div className="tb-section-head">
        <span className="knob-caption">Talkback</span>
      </div>
      <TalkKey warning={missing.length > 0 ? `Talkback goes nowhere: ${missing.join(', ')}` : null} />
      {missing.length > 0 && (
        <p className="tb-warning">
          <TriangleAlert size={13} /> {missing.join(' · ')}: nobody hears it.
        </p>
      )}
      <div className="tb-row">
        <Select
          value={talkback.input === null ? '' : String(talkback.input)}
          title="The interface input the talkback mic is on (it may also feed a channel)"
          className={talkback.input === null ? 'unset' : ''}
          onChange={(v) => set({ input: v === '' ? null : Number(v) })}
        >
          <option value="">No input</option>
          {Array.from({ length: Math.max(props.inputs, talkback.input !== null ? talkback.input + 1 : 0) }, (_, i) => (
            <option key={i} value={i}>
              In {i + 1}
              {i >= props.inputs ? ' (gone)' : ''}
            </option>
          ))}
        </Select>
        <span className="tb-level">
          <Knob
            value={dbToPosition(talkback.level_db)}
            min={0}
            max={1}
            defaultValue={dbToPosition(0)}
            size={30}
            hideValue
            label="Talkback level (double-click: 0 dB)"
            format={(p) => formatDb(positionToDb(p))}
            onChange={(p) =>
              actLatest('talkback', {
                cmd: 'set_talkback',
                talkback: { ...talkback, level_db: Math.round(positionToDb(p) * 10) / 10 },
              })
            }
          />
          <span className="value tb-db">{formatDb(talkback.level_db)}</span>
        </span>
        <button
          type="button"
          className={`pill tb-hpf${talkback.hpf ? ' on' : ''}`}
          aria-pressed={talkback.hpf}
          title="High-pass at 100 Hz: takes handling noise and boom off the talkback mic"
          onClick={() => set({ hpf: !talkback.hpf })}
        >
          HPF 100
        </button>
      </div>
      <div className="tb-meter" title="The talkback mic after its HPF and level, before TALK: check the mic without talking">
        <span className="tb-meter-label">Mic</span>
        <LevelBar strip={TALKBACK_KEY} width={290} />
      </div>
      <Destinations session={session} label="Talk to" to={to} onChange={(next) => set({ to: next })} />
      <p className="pe-note">
        While talkback is open the monitor dims by 20 dB (as Dim does; not added to it), so the mic does not feed back in
        your ears. The meter reads the mic before TALK, for a check without talking.
      </p>
    </section>
  )
}

/** TALK: hold for momentary; double-click or a long press latches; a
 *  press while latched releases. */
function TalkKey(props: { warning: string | null }) {
  const { talking } = useLiveTalk()
  const [latched, setLatched] = useState(false)
  const [holding, setHolding] = useState(false)
  const mode = useRef<'idle' | 'hold' | 'latched'>('idle')
  const lastDown = useRef(-Infinity)
  const latchedAt = useRef(0)
  const timer = useRef<number | undefined>(undefined)

  const talk = (on: boolean) => act({ cmd: 'talk', active: on })
  const latch = () => {
    window.clearTimeout(timer.current)
    mode.current = 'latched'
    latchedAt.current = performance.now()
    setLatched(true)
    setHolding(false)
  }
  const release = () => {
    window.clearTimeout(timer.current)
    mode.current = 'idle'
    setLatched(false)
    setHolding(false)
    talk(false)
  }

  // Someone else (another page, the console) closed talkback: unlatch here,
  // once a fresh latch has had time to show in the status.
  useEffect(() => {
    if (latched && !talking && performance.now() - latchedAt.current > LATCH_GRACE_MS) {
      mode.current = 'idle'
      setLatched(false)
    }
  }, [latched, talking])

  // A held key is let go if the page loses focus mid-press.
  useEffect(() => {
    const onBlur = () => {
      if (mode.current === 'hold') release()
    }
    window.addEventListener('blur', onBlur)
    return () => {
      window.removeEventListener('blur', onBlur)
      window.clearTimeout(timer.current)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const press = () => {
    if (mode.current === 'latched') {
      release()
      return
    }
    const now = performance.now()
    if (now - lastDown.current < DOUBLE_MS) {
      // The second press of a double-click: on, and stays on.
      lastDown.current = -Infinity
      talk(true)
      latch()
      return
    }
    lastDown.current = now
    mode.current = 'hold'
    setHolding(true)
    talk(true)
    timer.current = window.setTimeout(() => {
      if (mode.current === 'hold') latch()
    }, LONG_PRESS_MS)
  }
  const lift = () => {
    window.clearTimeout(timer.current)
    if (mode.current === 'hold') {
      mode.current = 'idle'
      setHolding(false)
      talk(false)
    }
  }

  const onPointerDown = (e: ReactPointerEvent<HTMLButtonElement>) => {
    if (e.button !== 0) return
    e.preventDefault()
    e.currentTarget.setPointerCapture(e.pointerId)
    press()
  }
  const onKeyDown = (e: ReactKeyboardEvent<HTMLButtonElement>) => {
    if ((e.key === ' ' || e.key === 'Enter') && !e.repeat) {
      e.preventDefault()
      press()
    }
  }
  const onKeyUp = (e: ReactKeyboardEvent<HTMLButtonElement>) => {
    if (e.key === ' ' || e.key === 'Enter') {
      e.preventDefault()
      lift()
    }
  }

  const on = latched || holding || talking
  const state = latched ? 'latched' : holding || talking ? 'talking' : 'idle'
  return (
    <button
      type="button"
      className={`talk-key ${state}${props.warning ? ' nowhere' : ''}`}
      aria-pressed={on}
      title={`${props.warning ? `${props.warning}. ` : ''}Hold to talk · double-click or long-press to latch · press again to release`}
      onPointerDown={onPointerDown}
      onPointerUp={lift}
      onPointerCancel={lift}
      onLostPointerCapture={lift}
      onKeyDown={onKeyDown}
      onKeyUp={onKeyUp}
      onContextMenu={(e) => e.preventDefault()}
    >
      <span className="talk-key-main">
        {latched && <Lock size={16} />}
        TALK
      </span>
      <span className="talk-key-hint">
        {latched
          ? 'Latched · press to release'
          : on
            ? 'Talking · let go to stop'
            : 'Hold to talk · double-click or long-press to latch'}
      </span>
    </button>
  )
}

// ── Oscillator ──────────────────────────────────────────────────────────

const OSC_KINDS: [OscillatorKind, string][] = [
  ['sine', 'Sine'],
  ['pink', 'Pink'],
  ['white', 'White'],
]

function OscillatorSection(props: { session: Session }) {
  const { session } = props
  const osc = session.oscillator
  const { oscillator: on } = useLiveTalk()
  const to = liveDestinations(session, osc.to)
  const set = (patch: Partial<Oscillator>) => act({ cmd: 'set_oscillator', oscillator: { ...osc, ...patch } })
  const off = osc.level_db <= OSC_LEVEL_DB[0]
  const names = destinations(session)
    .filter((d) => to.some((t) => sameStrip(t, d.to)))
    .map((d) => d.name)
  return (
    <section className={`tb-section osc${on ? ' on' : ''}`}>
      <div className="tb-section-head">
        <span className="knob-caption">Oscillator</span>
        <span className="spacer" />
        <button
          type="button"
          className={`osc-key${on ? ' on' : ''}`}
          aria-pressed={on}
          title={on ? 'The oscillator is sounding: click to stop it (it ramps out)' : 'Start the oscillator (it ramps in over 50 ms)'}
          onClick={() => act({ cmd: 'oscillator_on', on: !on })}
        >
          <AudioWaveform size={14} />
          {on ? `ON · ${off ? 'silent' : `${osc.level_db.toFixed(0)} dBFS`}` : 'OFF'}
        </button>
      </div>
      {on && (
        <p className="tb-warning osc-warning">
          <TriangleAlert size={13} />{' '}
          {to.length === 0 ? 'On, but sent nowhere.' : `Sounding into ${names.join(', ')}.`}
        </p>
      )}
      <div className="tb-row">
        <Choice value={osc.kind} options={OSC_KINDS} onChange={(kind) => set({ kind })} />
        {osc.kind === 'sine' && (
          <span className="tb-level">
            <Knob
              value={Math.log10(osc.hz)}
              min={Math.log10(OSC_HZ[0])}
              max={Math.log10(OSC_HZ[1])}
              defaultValue={3}
              size={30}
              hideValue
              label="Frequency (double-click: 1 kHz)"
              format={(v) => `${formatHz(10 ** v)} Hz`}
              onChange={(v) =>
                actLatest('oscillator', { cmd: 'set_oscillator', oscillator: { ...osc, hz: Math.round(10 ** v) } })
              }
            />
            <HzField hz={osc.hz} onCommit={(hz) => set({ hz })} />
          </span>
        )}
      </div>
      <div className="tb-row">
        <span className="tb-level">
          <Knob
            value={osc.level_db}
            min={OSC_LEVEL_DB[0]}
            max={OSC_LEVEL_DB[1]}
            defaultValue={-20}
            size={30}
            hideValue
            label="Level, dBFS (double-click: −20)"
            format={(v) => `${v.toFixed(0)} dBFS`}
            onChange={(v) =>
              actLatest('oscillator', {
                cmd: 'set_oscillator',
                oscillator: { ...osc, level_db: Math.round(v * 2) / 2 },
              })
            }
          />
          <span className={`value tb-db${on ? ' hot' : ''}`}>{off ? 'off' : `${osc.level_db.toFixed(1)} dBFS`}</span>
        </span>
        <span className="pe-note">−90 is silence. Line-up tone is usually −18 or −20 dBFS.</span>
      </div>
      <Destinations session={session} label="Send to" to={to} onChange={(next) => set({ to: next })} />
    </section>
  )
}

function HzField(props: { hz: number; onCommit: (hz: number) => void }) {
  return (
    <label className="cv-number tb-hz">
      <input
        key={props.hz}
        defaultValue={Math.round(props.hz)}
        inputMode="decimal"
        aria-label="Frequency in hertz"
        onBlur={(e) => {
          const value = Number(e.currentTarget.value)
          if (Number.isFinite(value) && value > 0) {
            const hz = Math.min(OSC_HZ[1], Math.max(OSC_HZ[0], value))
            if (hz !== props.hz) props.onCommit(hz)
          }
          e.currentTarget.value = String(Math.round(props.hz))
        }}
        onKeyDown={(e) => {
          if (e.key === 'Enter') e.currentTarget.blur()
          if (e.key === 'Escape') {
            e.currentTarget.value = String(Math.round(props.hz))
            e.currentTarget.blur()
          }
        }}
      />
      <span className="cv-number-unit">Hz</span>
    </label>
  )
}

/** A checkbox per place it can go: master, monitor, every bus and matrix. */
function Destinations(props: {
  session: Session
  label: string
  to: Destination[]
  onChange: (to: Destination[]) => void
}) {
  const { session } = props
  const all = destinations(session)
  const groups = [...new Set(all.map((d) => d.group))]
  return (
    <div className="tb-dests">
      <span className="knob-caption">
        {props.label} · {props.to.length}
      </span>
      {groups.map((group) => (
        <div key={group} className="tb-dest-group">
          <span className="tb-dest-group-name">{group}</span>
          <div className="tb-dest-grid">
            {all
              .filter((d) => d.group === group)
              .map((d) => {
                const on = props.to.some((t) => sameStrip(t, d.to))
                return (
                  <label key={stripKey(d.to)} className={`tb-dest${on ? ' on' : ''}`} title={`${group}: ${d.name}`}>
                    <input
                      type="checkbox"
                      checked={on}
                      onChange={() => props.onChange(toggleDestination(session, props.to, d.to, !on))}
                    />
                    <span className="tb-dest-name">{d.name}</span>
                  </label>
                )
              })}
          </div>
        </div>
      ))}
    </div>
  )
}
