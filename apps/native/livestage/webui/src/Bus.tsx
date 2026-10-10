// Buses and matrices (Phase 3): adding a bus with its role and width, a
// bus's settings, a matrix's sources (master and buses × level and pan),
// and the Sends on Fader banner over the mixer.

import { useState } from 'react'
import type { CSSProperties, ReactNode } from 'react'
import { Grid3x3, Layers, LogOut, SlidersVertical, TriangleAlert } from 'lucide-react'
import { Knob } from './controls.tsx'
import { Choice } from './editors/kit.tsx'
import { dbToPosition, formatDb, formatPan, positionToDb } from './faderLaw.ts'
import { Modal } from './Inserts.tsx'
import { colorVar } from './processing.ts'
import type { BusRole, BusStrip, MatrixSource, MatrixStrip, Session } from './protocol.ts'
import { stripKey } from './protocol.ts'
import type { SofTarget } from './routing.ts'
import { ROLES, matrixSend, matrixSources, nextBusName, nextMatrixName, roleInfo } from './routing.ts'
import { act, actLatest, useStore } from './store.ts'
import './bus.css'

/** The colour Sends on Fader tints with: the bus's own, else the accent
 *  (SoF is a live routing state). */
export function sofColor(color: number | null): string {
  return colorVar(color) ?? 'var(--accent)'
}

export function sofStyle(color: number | null): CSSProperties {
  return { '--sof-color': sofColor(color) } as CSSProperties
}

/** Where a strip's output goes, for a matrix: the outputs it is patched to. */
export function patchedOutputs(session: Session, source: { kind: 'matrix'; id: number }): string {
  const pairs = session.outputs
    .filter((p) => p.source.kind === source.kind && 'id' in p.source && p.source.id === source.id)
    .map((p) => (p.right === null ? `${p.left + 1}` : `${p.left + 1}/${p.right + 1}`))
  return pairs.length === 0 ? '' : `Out ${pairs.join(', ')}`
}

// ── Role and width ──────────────────────────────────────────────────────

const ROLE_OPTIONS = ROLES.map((r) => [r.role, r.label] as const)
const WIDTH_OPTIONS = [
  ['stereo', 'Stereo'],
  ['mono', 'Mono'],
] as const

/** "AUX", "GRP", "FX": a bus's role at a glance. */
export function RoleBadge(props: { role: BusRole }) {
  const info = roleInfo(props.role)
  return (
    <span className={`role-badge role-${props.role}`} title={info.detail}>
      {props.role === 'group' ? 'GRP' : info.label.toUpperCase()}
    </span>
  )
}

/** A bus's role, width and Sends on Fader: the Selected Channel's card. */
export function BusSettings(props: { bus: BusStrip; onSof: (target: SofTarget) => void }) {
  const { bus } = props
  const phase3 = useStore((s) => s.phase3)
  const old = phase3 === false
  const info = roleInfo(bus.role)
  return (
    <div className="bus-settings">
      <div className="bus-settings-row">
        <span className="knob-caption">Role</span>
        <Choice
          value={bus.role}
          options={ROLE_OPTIONS}
          title={old ? 'This LiveStage server predates bus roles' : undefined}
          onChange={(role) => role !== bus.role && act({ cmd: 'set_bus_role', bus: bus.id, role })}
        />
      </div>
      <p className="pe-note">{info.detail}</p>
      <div className="bus-settings-row">
        <span className="knob-caption">Width</span>
        <Choice
          value={bus.stereo ? 'stereo' : 'mono'}
          options={WIDTH_OPTIONS}
          onChange={(width) =>
            (width === 'stereo') !== bus.stereo && act({ cmd: 'set_bus_stereo', bus: bus.id, stereo: width === 'stereo' })
          }
        />
      </div>
      <p className="pe-note">
        {bus.stereo
          ? 'Stereo: each send has its own pan (or follows the channel’s).'
          : 'Mono: the sum of left and right, the same on both sides. Sends have no pan.'}
      </p>
      {old && (
        <p className="bus-old">
          <TriangleAlert size={13} /> This server predates bus roles and width: changes are refused.
        </p>
      )}
      <button
        type="button"
        className="button sof-enter"
        style={sofStyle(bus.color)}
        title={`Show every channel's send to ${bus.name} on the faders`}
        onClick={() => props.onSof({ kind: 'bus', id: bus.id })}
      >
        <SlidersVertical size={14} /> Sends on fader
      </button>
    </div>
  )
}

// ── Adding ──────────────────────────────────────────────────────────────

export function AddBusDialog(props: { session: Session; role: BusRole; onClose: () => void; onAdded?: (role: BusRole) => void }) {
  const phase3 = useStore((s) => s.phase3)
  const old = phase3 === false
  const [role, setRole] = useState<BusRole>(props.role)
  const [stereo, setStereo] = useState(true)
  const [name, setName] = useState(() => nextBusName(props.session, props.role))
  const [named, setNamed] = useState(false)
  const pick = (next: BusRole) => {
    setRole(next)
    if (!named) setName(nextBusName(props.session, next))
  }
  const ok = name.trim() !== ''
  const add = () => {
    if (!ok) return
    act({ cmd: 'add_bus', name: name.trim(), role, stereo })
    props.onAdded?.(role)
    props.onClose()
  }
  return (
    <Modal
      small
      icon={<Layers size={16} />}
      title="Add a bus"
      onClose={props.onClose}
      footer={
        <>
          <button type="button" className="button" onClick={props.onClose}>
            Cancel
          </button>
          <button type="button" className="button primary" disabled={!ok} onClick={add}>
            Add {roleInfo(role).label} bus
          </button>
        </>
      }
    >
      <label className="dialog-field">
        <span>Name</span>
        <input
          className="text-input"
          autoFocus
          value={name}
          onFocus={(e) => e.currentTarget.select()}
          onChange={(e) => {
            setName(e.currentTarget.value)
            setNamed(true)
          }}
          onKeyDown={(e) => e.key === 'Enter' && add()}
        />
      </label>
      <div className="dialog-field">
        <span>Role</span>
        <div className="role-cards" role="radiogroup" aria-label="Role">
          {ROLES.map((r) => (
            <button
              key={r.role}
              type="button"
              role="radio"
              aria-checked={role === r.role}
              className={`role-card${role === r.role ? ' on' : ''}`}
              disabled={old && r.role !== 'group'}
              onClick={() => pick(r.role)}
            >
              <span className="role-card-head">
                <RoleBadge role={r.role} /> {r.label}
                <span className="role-card-default">{r.preFader ? 'sends pre-fader' : 'sends post-fader'}</span>
              </span>
              <span className="role-card-detail">{r.detail}</span>
            </button>
          ))}
        </div>
      </div>
      <div className="dialog-field">
        <span>Width</span>
        <Choice
          value={stereo ? 'stereo' : 'mono'}
          options={WIDTH_OPTIONS}
          onChange={(w) => !old && setStereo(w === 'stereo')}
        />
        <span className="dialog-hint">
          {stereo ? 'Sends pan into it (or follow the channel’s pan).' : 'One signal on both sides: a mono wedge. Sends have no pan.'}
        </span>
      </div>
      {old && (
        <p className="bus-old">
          <TriangleAlert size={13} /> This LiveStage server predates bus roles and width: it adds a stereo bus that feeds the
          master. Update the server for aux and FX buses.
        </p>
      )}
    </Modal>
  )
}

export function AddMatrixDialog(props: { session: Session; onClose: () => void }) {
  const [stereo, setStereo] = useState(true)
  const [name, setName] = useState(() => nextMatrixName(props.session))
  const ok = name.trim() !== ''
  const add = () => {
    if (!ok) return
    act({ cmd: 'add_matrix', name: name.trim(), stereo })
    props.onClose()
  }
  return (
    <Modal
      small
      icon={<Grid3x3 size={16} />}
      title="Add a matrix"
      subtitle="A feed built from the master and buses: delay or fill zones, a record feed"
      onClose={props.onClose}
      footer={
        <>
          <button type="button" className="button" onClick={props.onClose}>
            Cancel
          </button>
          <button type="button" className="button primary" disabled={!ok} onClick={add}>
            Add matrix
          </button>
        </>
      }
    >
      <label className="dialog-field">
        <span>Name</span>
        <input
          className="text-input"
          autoFocus
          value={name}
          onFocus={(e) => e.currentTarget.select()}
          onChange={(e) => setName(e.currentTarget.value)}
          onKeyDown={(e) => e.key === 'Enter' && add()}
        />
      </label>
      <div className="dialog-field">
        <span>Width</span>
        <Choice value={stereo ? 'stereo' : 'mono'} options={WIDTH_OPTIONS} onChange={(w) => setStereo(w === 'stereo')} />
      </div>
      <span className="dialog-hint">
        It starts silent: give it sources (the master, buses), then patch it to an output on Patch → Outputs. Its delay
        (up to 1 s) is in its Selected Channel.
      </span>
    </Modal>
  )
}

// ── A matrix's sources ──────────────────────────────────────────────────

/** Master and every bus, each at its own level (and pan into a stereo
 *  matrix): what the matrix sums. */
export function MatrixSources(props: { session: Session; matrix: MatrixStrip; onSof?: (target: SofTarget) => void }) {
  const { session, matrix } = props
  const set = (source: MatrixSource, level_db: number, pan: number, which: 'level' | 'pan') =>
    actLatest(`msend${which === 'pan' ? 'pan' : ''}:${matrix.id}:${stripKey(source)}`, {
      cmd: 'set_matrix_send',
      matrix: matrix.id,
      source,
      level_db,
      pan,
    })
  const sources = matrixSources(session)
  return (
    <div className="matrix-sources">
      {sources.map(({ source, name, detail }) => {
        const send = matrixSend(matrix, source)
        const silent = send.level_db <= -90
        return (
          <div key={stripKey(source)} className={`msrc${silent ? ' silent' : ''}`}>
            <span className="msrc-name">
              <strong>{name}</strong>
              <span className="msrc-detail">{detail}</span>
            </span>
            <span className="msrc-level">
              <Knob
                value={dbToPosition(send.level_db)}
                min={0}
                max={1}
                defaultValue={dbToPosition(0)}
                size={30}
                hideValue
                label={`${name} into ${matrix.name} (double-click: 0 dB)`}
                format={(p) => formatDb(positionToDb(p))}
                onChange={(p) => set(source, Math.round(positionToDb(p) * 10) / 10, send.pan, 'level')}
              />
              <span className="value msrc-db">{formatDb(send.level_db)}</span>
            </span>
            {matrix.stereo ? (
              <span className="msrc-pan">
                <Knob
                  value={send.pan}
                  min={-1}
                  max={1}
                  defaultValue={0}
                  bipolar
                  size={26}
                  hideValue
                  label={`${name}'s pan into ${matrix.name} (double-click: centre)`}
                  format={formatPan}
                  onChange={(pan) => set(source, send.level_db, pan, 'pan')}
                />
                <span className="value msrc-db">{formatPan(send.pan)}</span>
              </span>
            ) : (
              <span className="msrc-pan msrc-mono" title="A mono matrix: no pan">
                mono
              </span>
            )}
          </div>
        )
      })}
      {session.buses.length === 0 && <span className="pe-note">Add buses to feed it more than the master.</span>}
    </div>
  )
}

/** A matrix's sources over the mixer (from its strip). */
export function MatrixDialog(props: {
  session: Session
  matrix: MatrixStrip
  onClose: () => void
  onSof: (target: SofTarget) => void
}) {
  const { matrix } = props
  return (
    <Modal
      icon={<Grid3x3 size={16} />}
      title={`${matrix.name}: sources`}
      subtitle={`${matrix.stereo ? 'Stereo' : 'Mono'} · the master and buses after their faders, each at its own level`}
      onClose={props.onClose}
      toolbar={
        <button
          type="button"
          className="button small sof-enter"
          style={sofStyle(matrix.color)}
          onClick={() => {
            props.onSof({ kind: 'matrix', id: matrix.id })
            props.onClose()
          }}
        >
          <SlidersVertical size={13} /> On faders
        </button>
      }
    >
      <MatrixSources session={props.session} matrix={matrix} onSof={props.onSof} />
    </Modal>
  )
}

// ── Sends on Fader banner ───────────────────────────────────────────────

/** Over the mixer while the faders are a bus's (or a matrix's) send
 *  levels: what they are, and the way out. */
export function SofBanner(props: { session: Session; target: SofTarget; onExit: () => void }) {
  const { session, target } = props
  const bus = target.kind === 'bus' ? session.buses.find((b) => b.id === target.id) : undefined
  const matrix = target.kind === 'matrix' ? session.matrices.find((m) => m.id === target.id) : undefined
  const strip = bus ?? matrix
  if (!strip) return null
  let detail: ReactNode
  if (bus) {
    const info = roleInfo(bus.role)
    detail = `${info.label}${bus.stereo ? '' : ' · mono'} · new sends ${info.preFader ? 'pre' : 'post'}-fader`
  } else {
    detail = `Matrix${matrix?.stereo ? '' : ' · mono'} · master and bus levels into it`
  }
  return (
    <div className="sof-banner" role="status" style={sofStyle(strip.color)}>
      <SlidersVertical size={15} className="sof-banner-icon" />
      <span className="sof-banner-text">
        Sends to <strong>{strip.name}</strong>
        <span className="sof-banner-sep"> — </span>
        <span className="sof-banner-what">faders are send levels</span>
      </span>
      <span className="sof-banner-detail">{detail}</span>
      <span className="spacer" />
      <button type="button" className="button small sof-exit" onClick={props.onExit} title="Back to the strips' own faders (Esc)">
        <LogOut size={13} /> Exit <kbd>Esc</kbd>
      </button>
    </div>
  )
}
