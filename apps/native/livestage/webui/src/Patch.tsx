// The patchbay, read as the desktop app's is: rows are where signal is
// needed, columns are the interface's sockets, a lit cell is a connection.
// Three routings, one page each, at the full size of the window:
//
// * Inputs: the interface's inputs into channels;
// * Outputs: the mixes (master, buses, channels direct) to the outputs;
// * Record: which strips record, and from where.

import { useState } from 'react'
import type { ReactNode } from 'react'
import { Cable, Check, Circle, Disc3, Eraser, Layers, ListOrdered, LogIn, LogOut, Mic, Speaker } from 'lucide-react'
import { LevelBar } from './controls.tsx'
import type { ChannelStrip, InputPatch, OutputPatch, PatchSource, RecordTap, Session, StripRef } from './protocol.ts'
import { sameStrip, stripKey } from './protocol.ts'
import { act } from './store.ts'

export type PatchTab = 'inputs' | 'outputs' | 'record'

export const PATCH_TABS: PatchTab[] = ['inputs', 'outputs', 'record']

/** 1/2, 3/4, … and a last single output when the count is odd. */
function outputPairs(outputs: number): [number, number | null][] {
  const pairs: [number, number | null][] = []
  for (let left = 0; left < outputs; left += 2) pairs.push([left, left + 1 < outputs ? left + 1 : null])
  return pairs
}

function pairLabel(left: number, right: number | null): string {
  return right === null ? `${left + 1}` : `${left + 1}/${right + 1}`
}

/** "In 3", "In 3/4", or nothing patched. */
function inputLabel(input: InputPatch): string {
  if (input.left === null) return '—'
  return input.right === null ? `In ${input.left + 1}` : `In ${input.left + 1}/${input.right + 1}`
}

/** What a recording does with this strip, as `recording_strips` in the
 *  engine decides: mono channels stay mono only when recorded at the input. */
function recordWidth(session: Session, strip: StripRef): 1 | 2 {
  if (strip.kind !== 'channel') return 2
  const channel = session.channels.find((c) => c.id === strip.id)
  if (!channel) return 2
  return channel.input.right !== null || session.recording.tap === 'post_inserts' ? 2 : 1
}

/** The file a strip records to, as the recorder names it (`file_stem`):
 *  characters a file name cannot hold become `_`, and a repeated name gets
 *  " 2", " 3"… in record order. */
function fileNames(session: Session): Map<string, string> {
  const extension = session.recording.format
  const strips: [StripRef, string][] = [
    ...session.channels.filter((c) => c.record_arm).map((c): [StripRef, string] => [{ kind: 'channel', id: c.id }, c.name]),
    ...session.buses.filter((b) => b.record_arm).map((b): [StripRef, string] => [{ kind: 'bus', id: b.id }, b.name]),
    ...(session.master.record_arm ? [[{ kind: 'master' }, 'Master'] as [StripRef, string]] : []),
  ]
  const used = new Set<string>()
  const names = new Map<string, string>()
  for (const [strip, name] of strips) {
    // eslint-disable-next-line no-control-regex
    const cleaned = name.replace(/[/\\:*?"<>|\u0000-\u001f]/g, '_').trim().replace(/\.+$/, '')
    const stem = cleaned || 'Track'
    let unique = stem
    for (let n = 2; used.has(unique.toLowerCase()); n++) unique = `${stem} ${n}`
    used.add(unique.toLowerCase())
    names.set(stripKey(strip), `${unique}.${extension}`)
  }
  return names
}

function Cell(props: {
  lit: boolean
  wide?: boolean
  hot?: boolean
  title: string
  onClick: () => void
  onHover: () => void
}) {
  return (
    <button
      type="button"
      className={`cell${props.wide ? ' wide' : ''}${props.lit ? ' lit' : ''}${props.hot ? ' hot' : ''}`}
      aria-pressed={props.lit}
      aria-label={props.title}
      title={props.title}
      onClick={props.onClick}
      onPointerEnter={props.onHover}
    >
      {props.lit && <Check size={14} strokeWidth={3} />}
    </button>
  )
}

function Empty(props: { children: ReactNode }) {
  return (
    <div className="patch-empty">
      <Cable size={22} />
      <span>{props.children}</span>
    </div>
  )
}

export function Patch(props: {
  session: Session
  inputs: number
  outputs: number
  recording: boolean
  tab: PatchTab
  onTab: (tab: PatchTab) => void
}) {
  const { session, inputs, outputs, tab } = props
  const patchedChannels = session.channels.filter((c) => c.input.left !== null).length
  const armed =
    session.channels.filter((c) => c.record_arm).length +
    session.buses.filter((b) => b.record_arm).length +
    (session.master.record_arm ? 1 : 0)

  const tabs: { id: PatchTab; label: string; icon: ReactNode; count: string; title: string }[] = [
    {
      id: 'inputs',
      label: 'Inputs',
      icon: <LogIn size={15} />,
      count: `${patchedChannels}/${session.channels.length}`,
      title: 'Channels with an input patched',
    },
    {
      id: 'outputs',
      label: 'Outputs',
      icon: <LogOut size={15} />,
      count: `${session.outputs.length}`,
      title: 'Connections to outputs',
    },
    {
      id: 'record',
      label: 'Record',
      icon: <Disc3 size={15} />,
      count: `${armed}`,
      title: 'Strips armed to record',
    },
  ]

  return (
    <div className="patch">
      <div className="patch-bar">
        <nav className="tabs" role="tablist" aria-label="Routing">
          {tabs.map(({ id, label, icon, count, title }) => (
            <button
              key={id}
              type="button"
              role="tab"
              aria-selected={tab === id}
              className={tab === id ? 'on' : ''}
              onClick={() => props.onTab(id)}
            >
              {icon}
              <span>{label}</span>
              <span className={`tab-count${id === 'record' && armed > 0 ? ' armed' : ''}`} title={title}>
                {count}
              </span>
            </button>
          ))}
        </nav>
      </div>
      {tab === 'inputs' ? (
        <InputsPage session={session} inputs={inputs} />
      ) : tab === 'outputs' ? (
        <OutputsPage session={session} outputs={outputs} />
      ) : (
        <RecordPage session={session} recording={props.recording} />
      )}
    </div>
  )
}

function PageHead(props: { title: string; text: string; children?: ReactNode }) {
  return (
    <div className="patch-head">
      <div>
        <h2>{props.title}</h2>
        <p>{props.text}</p>
      </div>
      {props.children && <div className="patch-actions">{props.children}</div>}
    </div>
  )
}

// ── Inputs ──────────────────────────────────────────────────────────────

function InputsPage(props: { session: Session; inputs: number }) {
  const { session, inputs } = props
  const [hot, setHot] = useState<number | null>(null)
  const sockets = Array.from({ length: inputs }, (_, i) => i)
  const users = (socket: number) =>
    session.channels.filter((c) => c.input.left === socket || c.input.right === socket)

  const setInput = (channel: ChannelStrip, input: InputPatch) =>
    act({ cmd: 'set_channel_input', channel: channel.id, input })

  // Channel n takes input n (mono), as far as both go.
  const oneToOne = () =>
    session.channels.forEach((channel, index) => {
      const input: InputPatch = index < inputs ? { left: index, right: null } : { left: null, right: null }
      if (channel.input.left !== input.left || channel.input.right !== input.right) setInput(channel, input)
    })
  const clear = () =>
    session.channels.forEach((channel) => {
      if (channel.input.left !== null) setInput(channel, { left: null, right: null })
    })

  return (
    <section className="patch-page">
      <PageHead
        title="Inputs to channels"
        text="Each channel takes one input, or two in stereo (ST). Several channels can share an input."
      >
        <button
          type="button"
          className="button"
          disabled={inputs === 0 || session.channels.length === 0}
          title="Channel 1 from input 1, channel 2 from input 2, … all mono"
          onClick={oneToOne}
        >
          <ListOrdered size={14} /> 1 : 1
        </button>
        <button type="button" className="button" disabled={patchedCount(session) === 0} onClick={clear}>
          <Eraser size={14} /> Clear
        </button>
      </PageHead>
      {inputs === 0 ? (
        <Empty>The interface has no inputs open. Pick an input device in Setup.</Empty>
      ) : session.channels.length === 0 ? (
        <Empty>No channels yet. Add one on the Mixer page.</Empty>
      ) : (
        <div className="patch-grid" onPointerLeave={() => setHot(null)}>
          <table className="matrix">
            <thead>
              <tr>
                <th className="matrix-corner">
                  <span>Channel</span>
                  <span className="matrix-corner-unit">Input</span>
                </th>
                {sockets.map((s) => {
                  const taken = users(s)
                  return (
                    <th
                      key={s}
                      className={`${hot === s ? 'hot' : ''}${taken.length > 0 ? ' used' : ''}`}
                      title={taken.length > 0 ? `In ${s + 1} → ${taken.map((c) => c.name).join(', ')}` : `In ${s + 1}`}
                    >
                      {s + 1}
                    </th>
                  )
                })}
                <th className="matrix-st">Stereo</th>
              </tr>
            </thead>
            <tbody>
              {session.channels.map((channel) => {
                const { left, right } = channel.input
                const stereo = right !== null
                return (
                  <tr key={channel.id}>
                    <th className="matrix-row">
                      <span className="matrix-name">
                        <Mic size={13} />
                        <span className="matrix-label">{channel.name}</span>
                      </span>
                      <span className="matrix-meta">
                        <LevelBar strip={stripKey({ kind: 'channel', id: channel.id })} tap="input" title="Input level" />
                        <span className="matrix-route">{inputLabel(channel.input)}</span>
                      </span>
                    </th>
                    {sockets.map((socket) => {
                      const lit = left === socket || right === socket
                      return (
                        <td key={socket}>
                          <Cell
                            lit={lit}
                            hot={hot === socket}
                            title={`${channel.name} ← In ${socket + 1}`}
                            onHover={() => setHot(socket)}
                            onClick={() =>
                              setInput(
                                channel,
                                lit
                                  ? { left: null, right: null }
                                  : stereo && socket + 1 < inputs
                                    ? { left: socket, right: socket + 1 }
                                    : { left: socket, right: null },
                              )
                            }
                          />
                        </td>
                      )
                    })}
                    <td className="matrix-st">
                      <button
                        type="button"
                        className={`pill${stereo ? ' on' : ''}`}
                        title="Stereo: take the next input too"
                        aria-pressed={stereo}
                        disabled={left === null || (!stereo && left + 1 >= inputs)}
                        onClick={() => {
                          if (left === null) return
                          setInput(channel, stereo ? { left, right: null } : { left, right: left + 1 })
                        }}
                      >
                        ST
                      </button>
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </table>
        </div>
      )}
    </section>
  )
}

function patchedCount(session: Session): number {
  return session.channels.filter((c) => c.input.left !== null).length
}

// ── Outputs ─────────────────────────────────────────────────────────────

function OutputsPage(props: { session: Session; outputs: number }) {
  const { session, outputs } = props
  const [hot, setHot] = useState<number | null>(null)
  const pairs = outputPairs(outputs)
  const sources: { name: string; group: string; source: PatchSource; icon: ReactNode }[] = [
    { name: 'Master', group: 'Mix', source: { kind: 'master' }, icon: <Speaker size={13} /> },
    ...session.buses.map((b) => ({
      name: b.name,
      group: 'Bus',
      source: { kind: 'bus', id: b.id } as PatchSource,
      icon: <Layers size={13} />,
    })),
    ...session.channels.map((c) => ({
      name: c.name,
      group: 'Direct',
      source: { kind: 'channel', id: c.id } as PatchSource,
      icon: <Mic size={13} />,
    })),
  ]
  const connected = (source: PatchSource, left: number, right: number | null) =>
    session.outputs.some((p) => sameStrip(p.source, source) && p.left === left && p.right === right)

  const toggleOutput = (source: PatchSource, left: number, right: number | null, lit: boolean) => {
    const next: OutputPatch[] = session.outputs.filter(
      (p) => !(sameStrip(p.source, source) && p.left === left && p.right === right),
    )
    if (!lit) next.push({ source, left, right })
    act({ cmd: 'set_output_patch', patches: next })
  }

  return (
    <section className="patch-page">
      <PageHead
        title="Mixes to outputs"
        text="A mix can go to several outputs, and an output can take several mixes. Direct outs send a channel on its own, after its fader."
      >
        <button
          type="button"
          className="button"
          disabled={pairs.length === 0 || connected({ kind: 'master' }, pairs[0][0], pairs[0][1])}
          title="The master to the first pair"
          onClick={() => toggleOutput({ kind: 'master' }, pairs[0][0], pairs[0][1], false)}
        >
          <Speaker size={14} /> Master → {pairs.length > 0 ? pairLabel(pairs[0][0], pairs[0][1]) : '—'}
        </button>
        <button
          type="button"
          className="button"
          disabled={session.outputs.length === 0}
          onClick={() => act({ cmd: 'set_output_patch', patches: [] })}
        >
          <Eraser size={14} /> Clear
        </button>
      </PageHead>
      {pairs.length === 0 ? (
        <Empty>The interface has no outputs open. Pick an output device in Setup.</Empty>
      ) : (
        <div className="patch-grid" onPointerLeave={() => setHot(null)}>
          <table className="matrix">
            <thead>
              <tr>
                <th className="matrix-corner">
                  <span>Mix</span>
                  <span className="matrix-corner-unit">Output</span>
                </th>
                {pairs.map(([l, r]) => {
                  const taken = session.outputs.filter((p) => p.left === l && p.right === r)
                  return (
                    <th key={l} className={`${hot === l ? 'hot' : ''}${taken.length > 0 ? ' used' : ''}`}>
                      {pairLabel(l, r)}
                    </th>
                  )
                })}
              </tr>
            </thead>
            <tbody>
              {sources.map(({ name, group, source, icon }, index) => (
                <tr key={stripKey(source)} className={index > 0 && sources[index - 1].group !== group ? 'group-start' : ''}>
                  <th className="matrix-row">
                    <span className="matrix-name">
                      {icon}
                      <span className="matrix-label">{name}</span>
                    </span>
                    <span className="matrix-meta">
                      <LevelBar strip={stripKey(source)} title="Level" />
                      <span className="matrix-route">{group}</span>
                    </span>
                  </th>
                  {pairs.map(([left, right]) => {
                    const lit = connected(source, left, right)
                    return (
                      <td key={left}>
                        <Cell
                          lit={lit}
                          wide
                          hot={hot === left}
                          title={`${name} → Out ${pairLabel(left, right)}`}
                          onHover={() => setHot(left)}
                          onClick={() => toggleOutput(source, left, right, lit)}
                        />
                      </td>
                    )
                  })}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  )
}

// ── Record ──────────────────────────────────────────────────────────────

function RecordPage(props: { session: Session; recording: boolean }) {
  const { session, recording } = props
  const names = fileNames(session)
  const tap = session.recording.tap
  const setTap = (next: RecordTap) => act({ cmd: 'set_record_settings', settings: { ...session.recording, tap: next } })
  const arm = (strip: StripRef, on: boolean) => act({ cmd: 'set_record_arm', strip, arm: on })

  const rows: { strip: StripRef; name: string; group: string; icon: ReactNode; source: string; on: boolean }[] = [
    ...session.channels.map((c) => ({
      strip: { kind: 'channel', id: c.id } as StripRef,
      name: c.name,
      group: 'Channel',
      icon: <Mic size={13} />,
      source: tap === 'input' ? inputLabel(c.input) : 'after inserts',
      on: c.record_arm,
    })),
    ...session.buses.map((b) => ({
      strip: { kind: 'bus', id: b.id } as StripRef,
      name: b.name,
      group: 'Bus',
      icon: <Layers size={13} />,
      source: 'after the fader',
      on: b.record_arm,
    })),
    {
      strip: { kind: 'master' },
      name: 'Master',
      group: 'Mix',
      icon: <Speaker size={13} />,
      source: 'after the fader',
      on: session.master.record_arm,
    },
  ]
  const channelsArmed = session.channels.every((c) => c.record_arm)

  return (
    <section className="patch-page">
      <PageHead
        title="Record routing"
        text={
          recording
            ? 'Recording now: arming applies to the next take.'
            : 'Each armed strip records to its own file in the take folder. Format and folder are in Setup.'
        }
      >
        <div className="segments" role="radiogroup" aria-label="Channels record">
          <button
            type="button"
            role="radio"
            aria-checked={tap === 'input'}
            className={tap === 'input' ? 'on' : ''}
            title="The channel's input after trim: a clean multitrack to mix again later"
            onClick={() => setTap('input')}
          >
            Channels at the input
          </button>
          <button
            type="button"
            role="radio"
            aria-checked={tap === 'post_inserts'}
            className={tap === 'post_inserts' ? 'on' : ''}
            title="What the inserts make of it, in stereo"
            onClick={() => setTap('post_inserts')}
          >
            After inserts
          </button>
        </div>
        <button
          type="button"
          className="button"
          disabled={session.channels.length === 0}
          onClick={() =>
            session.channels.forEach((c) => {
              if (c.record_arm === channelsArmed) arm({ kind: 'channel', id: c.id }, !channelsArmed)
            })
          }
        >
          <Circle size={13} /> {channelsArmed && session.channels.length > 0 ? 'Disarm channels' : 'Arm all channels'}
        </button>
      </PageHead>
      <div className="patch-grid">
        <table className="record-table">
          <thead>
            <tr>
              <th className="matrix-corner">Strip</th>
              <th>Arm</th>
              <th>Records</th>
              <th>Width</th>
              <th className="record-file">File</th>
            </tr>
          </thead>
          <tbody>
            {rows.map(({ strip, name, group, icon, source, on }, index) => {
              const width = recordWidth(session, strip)
              return (
                <tr
                  key={stripKey(strip)}
                  className={`${on ? 'armed' : ''}${index > 0 && rows[index - 1].group !== group ? ' group-start' : ''}`}
                >
                  <th className="matrix-row">
                    <span className="matrix-name">
                      {icon}
                      <span className="matrix-label">{name}</span>
                    </span>
                    <span className="matrix-meta">
                      <LevelBar
                        strip={stripKey(strip)}
                        tap={strip.kind === 'channel' && tap === 'input' ? 'input' : 'output'}
                        title="What it would record"
                      />
                      <span className="matrix-route">{group}</span>
                    </span>
                  </th>
                  <td>
                    <button
                      type="button"
                      className={`arm${on ? ' on' : ''}`}
                      aria-pressed={on}
                      title={on ? `Disarm ${name}` : `Arm ${name}`}
                      onClick={() => arm(strip, !on)}
                    >
                      <span className="arm-dot" />
                    </button>
                  </td>
                  <td className="record-source">{source}</td>
                  <td className="record-width">{width === 2 ? 'Stereo' : 'Mono'}</td>
                  <td className="record-file">{on ? names.get(stripKey(strip)) : <span className="faint">—</span>}</td>
                </tr>
              )
            })}
          </tbody>
        </table>
      </div>
    </section>
  )
}
