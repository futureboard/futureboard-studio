// The patchbay, read as the desktop app's is: rows are where signal is
// needed, columns are the interface's sockets, a lit cell is a connection.

import type { ReactNode } from 'react'
import { Cable, Check, Layers, LogIn, LogOut, Mic, Speaker } from 'lucide-react'
import type { InputPatch, OutputPatch, PatchSource, Session } from './protocol.ts'
import { sameStrip, stripKey } from './protocol.ts'
import { act } from './store.ts'

/** 1/2, 3/4, … and a last single output when the count is odd. */
function outputPairs(outputs: number): [number, number | null][] {
  const pairs: [number, number | null][] = []
  for (let left = 0; left < outputs; left += 2) pairs.push([left, left + 1 < outputs ? left + 1 : null])
  return pairs
}

function Cell(props: { lit: boolean; wide?: boolean; title: string; onClick: () => void }) {
  return (
    <button
      type="button"
      className={`cell${props.wide ? ' wide' : ''}${props.lit ? ' lit' : ''}`}
      aria-pressed={props.lit}
      title={props.title}
      onClick={props.onClick}
    >
      {props.lit && <Check size={13} strokeWidth={3} />}
    </button>
  )
}

export function Patch(props: { session: Session; inputs: number; outputs: number }) {
  const { session, inputs, outputs } = props
  const sockets = Array.from({ length: inputs }, (_, i) => i)
  const pairs = outputPairs(outputs)
  const sources: { name: string; source: PatchSource; icon: ReactNode }[] = [
    { name: 'Master', source: { kind: 'master' }, icon: <Speaker size={13} /> },
    ...session.buses.map((b) => ({ name: b.name, source: { kind: 'bus', id: b.id } as PatchSource, icon: <Layers size={13} /> })),
    ...session.channels.map((c) => ({
      name: `${c.name} direct`,
      source: { kind: 'channel', id: c.id } as PatchSource,
      icon: <Mic size={13} />,
    })),
  ]

  const toggleOutput = (source: PatchSource, left: number, right: number | null, lit: boolean) => {
    const next: OutputPatch[] = session.outputs.filter(
      (p) => !(sameStrip(p.source, source) && p.left === left && p.right === right),
    )
    if (!lit) next.push({ source, left, right })
    act({ cmd: 'set_output_patch', patches: next })
  }

  return (
    <div className="page">
      <section className="card">
        <header className="card-head">
          <LogIn size={16} />
          <div>
            <h2>Inputs to channels</h2>
            <p>Each channel takes one input, or two in stereo (ST).</p>
          </div>
        </header>
        {inputs === 0 ? (
          <div className="card-empty">
            <Cable size={20} />
            The interface has no inputs open. Pick an input device in Setup.
          </div>
        ) : session.channels.length === 0 ? (
          <div className="card-empty">
            <Cable size={20} />
            No channels yet. Add one on the Mixer page.
          </div>
        ) : (
          <div className="matrix-scroll">
            <table className="matrix">
              <thead>
                <tr>
                  <th />
                  {sockets.map((s) => (
                    <th key={s}>{s + 1}</th>
                  ))}
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
                        <Mic size={13} /> {channel.name}
                      </th>
                      {sockets.map((socket) => {
                        const lit = left === socket || right === socket
                        return (
                          <td key={socket}>
                            <Cell
                              lit={lit}
                              title={`${channel.name} ← In ${socket + 1}`}
                              onClick={() => {
                                const input: InputPatch = lit
                                  ? { left: null, right: null }
                                  : stereo && socket + 1 < inputs
                                    ? { left: socket, right: socket + 1 }
                                    : { left: socket, right: null }
                                act({ cmd: 'set_channel_input', channel: channel.id, input })
                              }}
                            />
                          </td>
                        )
                      })}
                      <td className="matrix-st">
                        <button
                          type="button"
                          className={`pill${stereo ? ' on' : ''}`}
                          title="Stereo: take the next input too"
                          disabled={left === null}
                          onClick={() => {
                            if (left === null) return
                            const input: InputPatch = stereo
                              ? { left, right: null }
                              : left + 1 < inputs
                                ? { left, right: left + 1 }
                                : channel.input
                            act({ cmd: 'set_channel_input', channel: channel.id, input })
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

      <section className="card">
        <header className="card-head">
          <LogOut size={16} />
          <div>
            <h2>Mixes to outputs</h2>
            <p>A mix can go to several outputs; an output can take several mixes.</p>
          </div>
        </header>
        <div className="matrix-scroll">
          <table className="matrix">
            <thead>
              <tr>
                <th />
                {pairs.map(([l, r]) => (
                  <th key={l}>{r === null ? l + 1 : `${l + 1}/${r + 1}`}</th>
                ))}
              </tr>
            </thead>
            <tbody>
              {sources.map(({ name, source, icon }) => (
                <tr key={stripKey(source)}>
                  <th className="matrix-row">
                    {icon} {name}
                  </th>
                  {pairs.map(([left, right]) => {
                    const lit = session.outputs.some(
                      (p) => sameStrip(p.source, source) && p.left === left && p.right === right,
                    )
                    return (
                      <td key={left}>
                        <Cell
                          lit={lit}
                          wide
                          title={`${name} → Out ${right === null ? left + 1 : `${left + 1}/${right + 1}`}`}
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
      </section>
    </div>
  )
}
