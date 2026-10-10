// Phase 4: Setup → Remote (admin). OSC on the network and MIDI controllers,
// both speaking one address space (`/ch/1/fader`). Each change is sent at
// once (as the recorder's settings are); the port number on Enter or when
// the field is left. MIDI learn: pick a target, move a control, done.

import { useEffect, useState } from 'react'
import type { ReactNode } from 'react'
import {
  Cable,
  Copy,
  Plug,
  Radio,
  RefreshCw,
  Trash2,
  TriangleAlert,
  Unplug,
  Wand2,
  X,
} from 'lucide-react'
import { Select } from './controls.tsx'
import type { MapMode, MidiMap, RemotePortStatus, RemoteSettings, RemoteStatus, Session } from './protocol.ts'
import { DCA_COUNT } from './protocol.ts'
import { explain, notify, request, useStore } from './store.ts'
import './access.css'

const DEFAULT_REMOTE: RemoteSettings = {
  osc: { enabled: false, port: 8000, feedback: true },
  midi: { inputs: [], outputs: [], feedback: true, maps: [] },
}

/** The settings as the server sent them, whole. */
function readRemote(raw: unknown): RemoteSettings {
  const r = (raw ?? {}) as Partial<RemoteSettings>
  const osc: Partial<RemoteSettings['osc']> = r.osc ?? {}
  const midi: Partial<RemoteSettings['midi']> = r.midi ?? {}
  return {
    osc: {
      enabled: osc.enabled === true,
      port: typeof osc.port === 'number' ? osc.port : DEFAULT_REMOTE.osc.port,
      feedback: osc.feedback !== false,
    },
    midi: {
      inputs: Array.isArray(midi.inputs) ? midi.inputs : [],
      outputs: Array.isArray(midi.outputs) ? midi.outputs : [],
      feedback: midi.feedback !== false,
      maps: Array.isArray(midi.maps) ? midi.maps : [],
    },
  }
}

export function midiLabel(map: MidiMap): string {
  const m = map.midi
  const kind = m.kind === 'cc' ? `CC ${m.number}` : m.kind === 'note' ? `Note ${m.number}` : 'Program change'
  return `Ch ${m.channel} · ${kind}`
}

/** `/ch/2/fader` in words, from the show (1-based positions). */
export function describeTarget(session: Session, address: string): string {
  const parts = address.split('/').filter(Boolean)
  const at = (list: { name: string }[], n: string) => list[Number(n) - 1]?.name ?? `#${n} (none)`
  const leaf = (p: string[]) => p.join(' ')
  switch (parts[0]) {
    case 'ch':
      if (parts[2] === 'send') return `${at(session.channels, parts[1])} → ${at(session.buses, parts[3])} ${leaf(parts.slice(4))}`
      return `${at(session.channels, parts[1])} ${leaf(parts.slice(2))}`
    case 'bus':
      return `${at(session.buses, parts[1])} ${leaf(parts.slice(2))}`
    case 'mtx':
      return `${at(session.matrices, parts[1])} ${leaf(parts.slice(2))}`
    case 'dca':
      return `${session.dcas[Number(parts[1]) - 1]?.name ?? `DCA ${parts[1]}`} ${leaf(parts.slice(2))}`
    case 'master':
      return `Master ${leaf(parts.slice(1))}`
    case 'scene':
      return `Scene ${leaf(parts.slice(1))}`
    case 'talk':
      return 'Talkback'
    case 'oscillator':
      return 'Oscillator on/off'
    case 'playback':
      return `Playback ${leaf(parts.slice(1))}`
    case 'vsc':
      return 'Virtual soundcheck'
    default:
      return address
  }
}

const CHEAT_SHEET = `Strips (1-based, in the mixer's order)
/ch/<n>/…  /bus/<n>/…  /mtx/<n>/…  /master/…  /dca/<n>/… (fader, mute)
Leaves
…/fader f 0..1 (fader law)   …/db f dB   …/mute i 0/1
…/pan f -1..1   …/solo i 0/1   …/name (read only)
Sends and matrix sources
/ch/<n>/send/<b>/fader | db | pan
/mtx/<n>/src/master/…   /mtx/<n>/src/bus/<b>/…   (fader, db, pan)
Console
/scene/recall i   /scene/next   /scene/previous
/talk i   /oscillator/on i   /vsc i
/playback/play   /playback/stop
Feedback
/livestage/subscribe — current values at once, then changes, for 60 s (send again to renew; 8 at most)`

export function RemoteCard(props: { session: Session }) {
  const pushed = useStore((s) => s.remote)
  const learned = useStore((s) => s.learned)
  const [remote, setRemote] = useState<RemoteSettings | null>(null)
  const [status, setStatus] = useState<RemoteStatus | null>(null)
  const [ports, setPorts] = useState<{ inputs: string[]; outputs: string[] } | null>(null)
  const [portsError, setPortsError] = useState<string | null>(null)
  const [scanning, setScanning] = useState(false)
  const [port, setPort] = useState('')

  const read = async () => {
    const reply = await request({ cmd: 'remote_settings' })
    if (!reply.ok) {
      notify(explain(reply.error), true)
      return
    }
    const next = readRemote(reply.remote)
    setRemote(next)
    setPort(String(next.osc.port))
    setStatus((reply.status as RemoteStatus | undefined) ?? null)
  }
  const scan = async () => {
    setScanning(true)
    const reply = await request({ cmd: 'midi_ports' })
    setScanning(false)
    if (reply.ok) {
      setPorts({
        inputs: Array.isArray(reply.inputs) ? (reply.inputs as string[]) : [],
        outputs: Array.isArray(reply.outputs) ? (reply.outputs as string[]) : [],
      })
      setPortsError(null)
    } else setPortsError(explain(reply.error))
  }
  useEffect(() => {
    void read()
    void scan()
  }, [])
  // Pushed by the server (this or another admin's change, a learn, a port
  // that came or went): take it.
  useEffect(() => {
    if (!pushed) return
    setRemote(readRemote(pushed.settings))
    setPort(String(pushed.settings.osc.port))
    if (pushed.status) setStatus(pushed.status)
  }, [pushed])
  // A learn added a map: make sure the maps shown hold it.
  useEffect(() => {
    if (learned?.map) void read()
  }, [learned])

  const save = async (next: RemoteSettings) => {
    const before = remote
    setRemote(next)
    const reply = await request({ cmd: 'set_remote', remote: next })
    if (!reply.ok) {
      notify(explain(reply.error), true)
      setRemote(before)
      if (before) setPort(String(before.osc.port))
    } else {
      if (reply.remote) setRemote(readRemote(reply.remote))
      if (reply.status) setStatus(reply.status as RemoteStatus)
    }
  }

  if (!remote) {
    return (
      <section className="card">
        <RemoteHead />
        <p className="muted">Reading the remote settings…</p>
      </section>
    )
  }
  const { osc, midi } = remote
  const commitPort = () => {
    const n = Number(port)
    if (!Number.isInteger(n) || n < 1 || n > 65535) {
      notify('An OSC port is a whole number from 1 to 65535', true)
      setPort(String(osc.port))
      return
    }
    if (n !== osc.port) void save({ ...remote, osc: { ...osc, port: n } })
  }
  const togglePort = (side: 'inputs' | 'outputs', name: string, on: boolean) =>
    void save({
      ...remote,
      midi: { ...midi, [side]: on ? [...midi[side].filter((p) => p !== name), name] : midi[side].filter((p) => p !== name) },
    })

  return (
    <section className="card">
      <RemoteHead />

      <div className="remote-block">
        <div className="remote-block-head">
          <Radio size={14} />
          <strong>OSC</strong>
          <span className="muted">UDP, on the same address as this page</span>
          <span className="spacer" />
          <button
            type="button"
            className={`switch${osc.enabled ? ' on' : ''}`}
            aria-pressed={osc.enabled}
            onClick={() => void save({ ...remote, osc: { ...osc, enabled: !osc.enabled } })}
          >
            {osc.enabled ? 'On' : 'Off'}
          </button>
        </div>
        {osc.enabled && (
          <p className="remote-warning">
            <TriangleAlert size={14} /> With OSC on, anyone who can reach this console on the network can control the mixer,
            without a login (it acts as an engineer: no users, storage or system settings).
          </p>
        )}
        <div className="remote-row">
          <label className="remote-field">
            <span>Port</span>
            <input
              className="text-input remote-port value"
              inputMode="numeric"
              value={port}
              onChange={(e) => setPort(e.currentTarget.value)}
              onBlur={commitPort}
              onKeyDown={(e) => {
                if (e.key === 'Enter') e.currentTarget.blur()
                if (e.key === 'Escape') {
                  setPort(String(osc.port))
                  e.currentTarget.blur()
                }
              }}
            />
          </label>
          <button
            type="button"
            className={`pill remote-pill${osc.feedback ? ' on' : ''}`}
            aria-pressed={osc.feedback}
            title="Send changes back to subscribed OSC clients (/livestage/subscribe)"
            onClick={() => void save({ ...remote, osc: { ...osc, feedback: !osc.feedback } })}
          >
            FEEDBACK
          </button>
          <OscState enabled={osc.enabled} status={status} />
        </div>
        <CheatSheet />
      </div>

      <div className="remote-block">
        <div className="remote-block-head">
          <Cable size={14} />
          <strong>MIDI</strong>
          <span className="muted">Controllers and motor faders, by port name</span>
          <span className="spacer" />
          <button
            type="button"
            className={`pill remote-pill${midi.feedback ? ' on' : ''}`}
            aria-pressed={midi.feedback}
            title="Send mapped changes out to every open output (motor faders, LEDs)"
            onClick={() => void save({ ...remote, midi: { ...midi, feedback: !midi.feedback } })}
          >
            FEEDBACK
          </button>
          <button type="button" className="button ghost" disabled={scanning} onClick={() => void scan()}>
            <RefreshCw size={14} className={scanning ? 'spin' : ''} /> Rescan
          </button>
        </div>
        {(portsError ?? status?.midi.error) && (
          <p className="remote-warning">
            <TriangleAlert size={14} /> {portsError ?? status?.midi.error}
          </p>
        )}
        <div className="remote-ports">
          <PortList
            title="Inputs"
            icon={<Plug size={13} />}
            present={ports?.inputs ?? null}
            chosen={midi.inputs}
            states={status?.midi.inputs ?? []}
            onToggle={(name, on) => togglePort('inputs', name, on)}
          />
          <PortList
            title="Outputs"
            icon={<Unplug size={13} />}
            present={ports?.outputs ?? null}
            chosen={midi.outputs}
            states={status?.midi.outputs ?? []}
            onToggle={(name, on) => togglePort('outputs', name, on)}
          />
        </div>
        <MapsTable
          session={props.session}
          maps={midi.maps}
          onRemove={(i) => void save({ ...remote, midi: { ...midi, maps: midi.maps.filter((_, j) => j !== i) } })}
          onMode={(i, mode) =>
            void save({ ...remote, midi: { ...midi, maps: midi.maps.map((m, j) => (j === i ? { ...m, mode } : m)) } })
          }
        />
        <Learn session={props.session} inputs={midi.inputs.length} serverLearning={status?.learning ?? null} />
        {status && (
          <p className="remote-counters value" title="Since the server started">
            Remote events dropped (queue full) {status.dropped} · ignored (unknown address or type) {status.ignored} ·
            refused {status.refused}
          </p>
        )}
      </div>
    </section>
  )
}

function RemoteHead() {
  return (
    <header className="card-head">
      <Radio size={16} />
      <div>
        <h2>Remote control</h2>
        <p>OSC apps and MIDI controllers. They act as an engineer would: the mix, scenes, talkback and playback.</p>
      </div>
    </header>
  )
}

/** Whether OSC is listening, where, and what it has heard. */
function OscState(props: { enabled: boolean; status: RemoteStatus | null }) {
  const osc = props.status?.osc
  if (!osc) return null
  if (osc.error) {
    return (
      <span className="remote-diag error">
        <TriangleAlert size={13} /> {osc.error}
      </span>
    )
  }
  if (!props.enabled || !osc.listening) return <span className="remote-diag">Not listening</span>
  return (
    <span className="remote-diag" title="Since OSC was turned on">
      <span>
        Listening on <span className="value">{osc.listening}</span>
      </span>
      <span>
        {osc.subscribers} subscriber{osc.subscribers === 1 ? '' : 's'}
      </span>
      <span>
        received <span className="value">{osc.received}</span>
      </span>
      {osc.unreadable > 0 && (
        <span>
          not OSC <span className="value">{osc.unreadable}</span>
        </span>
      )}
    </span>
  )
}

function CheatSheet() {
  const [copied, setCopied] = useState(false)
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(CHEAT_SHEET)
      setCopied(true)
      window.setTimeout(() => setCopied(false), 1500)
    } catch {
      notify('This browser did not allow copying: select the text instead', true)
    }
  }
  return (
    <details className="remote-sheet">
      <summary>Address cheat-sheet</summary>
      <div className="remote-sheet-body">
        <pre>{CHEAT_SHEET}</pre>
        <button type="button" className="button ghost" onClick={() => void copy()}>
          <Copy size={14} /> {copied ? 'Copied' : 'Copy'}
        </button>
      </div>
    </details>
  )
}

function PortList(props: {
  title: string
  icon: ReactNode
  /** Null: not listed yet. */
  present: string[] | null
  chosen: string[]
  /** The server's view of each chosen port: open, or why not. */
  states: RemotePortStatus[]
  onToggle: (name: string, on: boolean) => void
}) {
  const names = [...new Set([...(props.present ?? []), ...props.chosen])]
  return (
    <div className="remote-port-list">
      <span className="knob-caption">
        {props.icon} {props.title}
      </span>
      {props.present === null ? (
        <span className="muted">Listing…</span>
      ) : names.length === 0 ? (
        <span className="muted">No MIDI {props.title.toLowerCase()} on this machine.</span>
      ) : (
        names.map((name) => {
          const on = props.chosen.includes(name)
          const state = props.states.find((s) => s.name === name)
          // Ticked but not open (unplugged, or busy): the server retries.
          const missing = on && (state ? !state.open : !props.present!.includes(name))
          return (
            <label
              key={name}
              className={`tb-dest${on ? ' on' : ''}${missing ? ' missing' : ''}`}
              title={missing ? `${name}: ${state?.error ?? 'not found'} — retried every 2 s` : name}
            >
              <input type="checkbox" checked={on} onChange={() => props.onToggle(name, !on)} />
              <span className="tb-dest-name">{name}</span>
              {missing && <span className="remote-missing">missing · retried</span>}
            </label>
          )
        })
      )}
    </div>
  )
}

const MODES: [MapMode, string][] = [
  ['absolute', 'Absolute'],
  ['toggle', 'Toggle'],
  ['momentary', 'Momentary'],
]

function MapsTable(props: {
  session: Session
  maps: MidiMap[]
  onRemove: (index: number) => void
  onMode: (index: number, mode: MapMode) => void
}) {
  if (props.maps.length === 0) {
    return <p className="card-empty remote-empty">No MIDI maps yet: learn one below.</p>
  }
  return (
    <div className="remote-maps">
      <table className="record-table">
        <thead>
          <tr>
            <th>MIDI</th>
            <th>Target</th>
            <th>Mode</th>
            <th aria-label="Remove" />
          </tr>
        </thead>
        <tbody>
          {props.maps.map((map, i) => (
            <tr key={`${midiLabel(map)}:${map.target}:${i}`}>
              <td className="value">{midiLabel(map)}</td>
              <td>
                <code>{map.target}</code>
                <span className="muted remote-target-name"> {describeTarget(props.session, map.target)}</span>
              </td>
              <td>
                <Select value={map.mode} onChange={(v) => props.onMode(i, v as MapMode)}>
                  {MODES.map(([m, label]) => (
                    <option key={m} value={m}>
                      {label}
                    </option>
                  ))}
                </Select>
              </td>
              <td>
                <button
                  type="button"
                  className="icon-button large danger"
                  title="Remove this map"
                  aria-label="Remove this map"
                  onClick={() => props.onRemove(i)}
                >
                  <Trash2 size={14} />
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

// ── Learn ───────────────────────────────────────────────────────────────

type Area = 'ch' | 'bus' | 'mtx' | 'master' | 'dca' | 'scene' | 'talk' | 'oscillator' | 'playback' | 'vsc'

const AREAS: [Area, string][] = [
  ['ch', 'Channel'],
  ['bus', 'Bus'],
  ['mtx', 'Matrix'],
  ['master', 'Master'],
  ['dca', 'DCA'],
  ['scene', 'Scene'],
  ['talk', 'Talkback'],
  ['oscillator', 'Oscillator'],
  ['playback', 'Playback'],
  ['vsc', 'Virtual soundcheck'],
]

const STRIP_LEAVES = ['fader', 'mute', 'pan', 'solo']

function leavesFor(area: Area): string[] {
  switch (area) {
    case 'dca':
      return ['fader', 'mute']
    case 'scene':
      return ['recall', 'next', 'previous']
    case 'playback':
      return ['play', 'stop']
    // The master has no solo.
    case 'master':
      return ['fader', 'mute', 'pan']
    case 'ch':
    case 'bus':
    case 'mtx':
      return STRIP_LEAVES
    default:
      return []
  }
}

function defaultMode(area: Area, leaf: string): MapMode {
  if (area === 'talk') return 'momentary'
  if (leaf === 'fader' || leaf === 'pan' || leaf === 'recall') return 'absolute'
  return 'toggle'
}

/** The address a learn target names. */
function address(area: Area, n: number, leaf: string): string {
  switch (area) {
    case 'ch':
    case 'bus':
    case 'mtx':
    case 'dca':
      return `/${area}/${n}/${leaf}`
    case 'master':
      return `/master/${leaf}`
    case 'scene':
      return `/scene/${leaf}`
    case 'talk':
      return '/talk'
    case 'oscillator':
      return '/oscillator/on'
    case 'playback':
      return `/playback/${leaf}`
    case 'vsc':
      return '/vsc'
  }
}

function Learn(props: {
  session: Session
  inputs: number
  /** The learn the server is waiting on (from another page, or this one). */
  serverLearning: { target: string; mode: MapMode } | null
}) {
  const { session } = props
  const learned = useStore((s) => s.learned)
  const [area, setArea] = useState<Area>('ch')
  const [n, setN] = useState(1)
  const [leaf, setLeaf] = useState('fader')
  const [mode, setMode] = useState<MapMode>('absolute')
  const [listening, setListening] = useState<{ target: string; since: number } | null>(null)
  const [result, setResult] = useState<MidiMap | null>(null)
  const [failure, setFailure] = useState<string | null>(null)

  const strips: { name: string }[] =
    area === 'ch'
      ? session.channels
      : area === 'bus'
        ? session.buses
        : area === 'mtx'
          ? session.matrices
          : area === 'dca'
            ? session.dcas.slice(0, DCA_COUNT)
            : []
  const leaves = leavesFor(area)
  const target = address(area, n, leaf)

  // The server pushed the map the learn produced (or why it took none).
  useEffect(() => {
    if (!learned || (listening && learned.at < listening.since)) return
    if (!listening && !props.serverLearning && Date.now() - learned.at > 2000) return
    setResult(learned.map)
    setFailure(learned.map ? null : (learned.error ?? 'The MIDI message could not be mapped'))
    setListening(null)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [learned])
  // A learn the server waits on, started elsewhere (or before a reload).
  const waiting = listening ?? (props.serverLearning ? { target: props.serverLearning.target, since: 0 } : null)

  const pickArea = (next: Area) => {
    setArea(next)
    setN(1)
    const first = leavesFor(next)[0] ?? ''
    setLeaf(first)
    setMode(defaultMode(next, first))
  }
  const start = async () => {
    setResult(null)
    setFailure(null)
    const since = Date.now()
    const reply = await request({ cmd: 'midi_learn', target, mode })
    if (reply.ok) setListening({ target, since })
    else notify(explain(reply.error), true)
  }
  const cancel = async () => {
    setListening(null)
    const reply = await request({ cmd: 'midi_learn_cancel' })
    if (!reply.ok) notify(explain(reply.error), true)
  }

  return (
    <div className={`remote-learn${waiting ? ' listening' : ''}`}>
      <span className="knob-caption">
        <Wand2 size={13} /> Learn a map
      </span>
      <div className="remote-learn-row">
        <Select value={area} onChange={(v) => pickArea(v as Area)} title="What the control drives">
          {AREAS.map(([a, label]) => (
            <option key={a} value={a}>
              {label}
            </option>
          ))}
        </Select>
        {strips.length > 0 && (
          <Select value={String(n)} onChange={(v) => setN(Number(v))} title="Which one (its place in the mixer)">
            {strips.map((s, i) => (
              <option key={i} value={i + 1}>
                {i + 1} · {s.name}
              </option>
            ))}
          </Select>
        )}
        {leaves.length > 0 && (
          <Select
            value={leaf}
            onChange={(v) => {
              setLeaf(v)
              setMode(defaultMode(area, v))
            }}
          >
            {leaves.map((l) => (
              <option key={l} value={l}>
                {l}
              </option>
            ))}
          </Select>
        )}
        <Select value={mode} onChange={(v) => setMode(v as MapMode)} title="How the MIDI value drives it">
          {MODES.map(([m, label]) => (
            <option key={m} value={m}>
              {label}
            </option>
          ))}
        </Select>
        <code className="remote-learn-target">{target}</code>
        <span className="spacer" />
        {waiting ? (
          <button type="button" className="button" onClick={() => void cancel()}>
            <X size={14} /> Cancel
          </button>
        ) : (
          <button
            type="button"
            className="button primary"
            disabled={(strips.length === 0 && ['ch', 'bus', 'mtx'].includes(area))}
            onClick={() => void start()}
          >
            <Wand2 size={14} /> Learn
          </button>
        )}
      </div>
      {waiting ? (
        <p className="remote-listening" role="status">
          <span className="remote-pulse" /> Move a control on your MIDI device… ({describeTarget(session, waiting.target)})
          {props.inputs === 0 && ' — no MIDI input is ticked above, so nothing can arrive.'}
        </p>
      ) : failure ? (
        <p className="remote-learned error" role="alert">
          <TriangleAlert size={13} /> {failure}
        </p>
      ) : result ? (
        <p className="remote-learned" role="status">
          Learned: <strong className="value">{midiLabel(result)}</strong> → <code>{result.target}</code> (
          {describeTarget(session, result.target)}, {result.mode})
        </p>
      ) : (
        <p className="dialog-hint">The next message from any ticked input becomes the map (replacing one on the same message).</p>
      )}
    </div>
  )
}
