// The console core around the strips: the layer switch and DCA spill, the
// eight mute groups, the monitor (solo mode, level, dim, clear solo), the
// DCA faders, and the colour swatches strips and DCAs share.

import { useEffect, useRef, useState } from 'react'
import type { CSSProperties } from 'react'
import { ArrowRight, Ban, Check, ChevronDown, Ear, Eye, Headphones, Pencil, TriangleAlert, X } from 'lucide-react'
import { BankBar } from './Banks.tsx'
import { Fader, Knob, Latch, LevelBar, Select } from './controls.tsx'
import { dbToPosition, formatDb, positionToDb } from './faderLaw.ts'
import { colorVar } from './processing.ts'
import type { Dca, Monitor, MonitorSource, Session, SoloMode, StripRef } from './protocol.ts'
import { MONITOR_KEY, STRIP_COLORS, stripKey } from './protocol.ts'
import type { Bank } from './routing.ts'
import { ROLES } from './routing.ts'
import { act, actLatest, useStore } from './store.ts'
import { MenuButton, SelectionCluster, SelectModeButton } from './Workflow.tsx'
import './console.css'

/** A strip colour as the CSS custom property strips and DCAs tint from. */
export function stripColorStyle(color: number | null): CSSProperties | undefined {
  const value = colorVar(color)
  return value ? ({ '--strip-color': value } as CSSProperties) : undefined
}

/** A DCA's colour on a chip or button inside a coloured strip: its own
 *  property, always set, so it never inherits the strip's. */
export function chipColorStyle(color: number | null): CSSProperties {
  return { '--chip-color': colorVar(color) ?? 'transparent' } as CSSProperties
}

/** Every channel and bus that follows DCA `dca`. */
export function dcaMembers(session: Session, dca: number): StripRef[] {
  return [
    ...session.channels.filter((c) => c.dcas.includes(dca)).map((c): StripRef => ({ kind: 'channel', id: c.id })),
    ...session.buses.filter((b) => b.dcas.includes(dca)).map((b): StripRef => ({ kind: 'bus', id: b.id })),
  ]
}

function muteGroupMembers(session: Session, group: number): number {
  return (
    session.channels.filter((c) => c.mute_groups.includes(group)).length +
    session.buses.filter((b) => b.mute_groups.includes(group)).length +
    session.matrices.filter((m) => m.mute_groups.includes(group)).length
  )
}

/** Whether any output takes the monitor bus. In PFL and AFL a solo is
 *  heard only there, so without one soloing is silent. */
export function monitorPatched(session: Session): boolean {
  return session.outputs.some((p) => p.source.kind === 'monitor')
}

/** What a solo button should add to its title: where the solo is heard, and
 *  that it is heard nowhere while the monitor is unpatched. */
export function soloHint(session: Session): string {
  const mode = session.monitor.solo_mode
  if (mode === 'sip') return 'solo in place: the PA hears only soloed strips'
  const name = mode === 'pfl' ? 'PFL' : 'AFL'
  return monitorPatched(session)
    ? `${name}: heard on the monitor bus; the PA is untouched`
    : `${name}: the monitor bus is not patched to an output, so solo is silent (Patch → Outputs)`
}

export function soloedCount(session: Session): number {
  return (
    session.channels.filter((c) => c.solo).length +
    session.buses.filter((b) => b.solo).length +
    session.matrices.filter((m) => m.solo).length
  )
}

// ── Colour ──────────────────────────────────────────────────────────────

/** Studio's twelve track colours and "none". */
export function ColorSwatches(props: { value: number | null; onChange: (color: number | null) => void; small?: boolean }) {
  return (
    <div className={`swatches${props.small ? ' small' : ''}`} role="radiogroup" aria-label="Colour">
      <button
        type="button"
        role="radio"
        aria-checked={props.value === null}
        className={`swatch none${props.value === null ? ' on' : ''}`}
        title="No colour"
        onClick={() => props.onChange(null)}
      >
        <Ban size={props.small ? 10 : 12} />
      </button>
      {Array.from({ length: STRIP_COLORS }, (_, i) => (
        <button
          key={i}
          type="button"
          role="radio"
          aria-checked={props.value === i}
          className={`swatch${props.value === i ? ' on' : ''}`}
          style={{ background: colorVar(i) ?? undefined }}
          title={`Colour ${i + 1}`}
          onClick={() => props.onChange(i)}
        >
          {props.value === i && <Check size={props.small ? 10 : 12} strokeWidth={3} />}
        </button>
      ))}
    </div>
  )
}

// ── The bar over the mixer ──────────────────────────────────────────────

export function ConsoleBar(props: {
  session: Session
  bank: Bank
  onBank: (bank: Bank) => void
  spill: number | null
  onSpill: (dca: number | null) => void
}) {
  const { session, spill } = props
  const consoleCore = useStore((s) => s.consoleCore)
  const spilled = spill !== null ? session.dcas[spill] : null
  const strips = props.bank !== 'dcas'
  return (
    <div className="console-bar">
      <div className="console-cluster banks">
        <BankBar session={session} bank={props.bank} onBank={props.onBank} />
        {strips && <SelectModeButton />}
        {spilled && spill !== null && (
          <span className="spill-chip" style={stripColorStyle(spilled.color)}>
            <Eye size={13} />
            <span className="spill-text">
              Spill · <strong>{spilled.name}</strong> · {dcaMembers(session, spill).length}
            </span>
            <button
              type="button"
              className="icon-button"
              title="Show every strip"
              aria-label="Show every strip"
              onClick={() => props.onSpill(null)}
            >
              <X size={13} />
            </button>
          </span>
        )}
      </div>
      {strips && <SelectionCluster session={session} />}
      <MuteGroups session={session} />
      <span className="spacer" />
      <MonitorControls session={session} />
      {consoleCore === false && (
        <span
          className="status-pill warn console-missing"
          title="This LiveStage server predates the console core: processing, DCAs, mute groups and the monitor show defaults and changes are refused."
        >
          <TriangleAlert size={13} />
          <span className="status-text">Server has no console core</span>
        </span>
      )}
    </div>
  )
}

function muteGroupTitle(session: Session, i: number): string {
  const group = session.mute_groups[i]
  const members = muteGroupMembers(session, i)
  return `${group.name}: ${members} strip${members === 1 ? '' : 's'}${group.active ? ', muted' : ''}. Mutes their outputs; pre-fader sends and PFL keep going`
}

/** Eight mute groups, lit while active. On a wide bar, eight number keys
 *  (the name on hover) and a list; on a narrower one, the list only, behind
 *  a key that counts the active groups. The list names each group, and can
 *  rename them. */
function MuteGroups(props: { session: Session }) {
  const { session } = props
  const active = session.mute_groups.filter((g) => g.active).length
  return (
    <div className="console-cluster mute-groups" role="group" aria-label="Mute groups">
      <span className="cluster-label mg-label">Mute</span>
      <div className="mg-grid">
        {session.mute_groups.map((group, i) => (
          <button
            key={i}
            type="button"
            className={`mg${group.active ? ' on' : ''}${muteGroupMembers(session, i) === 0 ? ' unused' : ''}`}
            aria-pressed={group.active}
            aria-label={`Mute group ${i + 1}, ${group.name}`}
            title={muteGroupTitle(session, i)}
            onClick={() => act({ cmd: 'set_mute_group', group: i, active: !group.active })}
          >
            <span className="mg-num">{i + 1}</span>
          </button>
        ))}
      </div>
      <MenuButton
        popover
        popClass="mg-pop"
        className={`mg-more${active > 0 ? ' on' : ''}`}
        label={`Mute groups${active > 0 ? `: ${active} active` : ''} · names and renaming`}
        icon={null}
        text={
          <>
            <span className="mg-more-label">Mute</span>
            {active > 0 && <span className="mg-more-count value">{active}</span>}
            <ChevronDown size={12} className="mg-more-chevron" />
          </>
        }
      >
        {() => <MuteGroupList session={session} />}
      </MenuButton>
    </div>
  )
}

/** Every mute group by name, as latches; "Rename" turns the names into
 *  fields. */
function MuteGroupList(props: { session: Session }) {
  const { session } = props
  const [naming, setNaming] = useState(false)
  // Names typed and not yet sent: sent on blur, or when the list closes
  // under a field (a press outside unmounts it before it blurs).
  const drafts = useRef(new Map<number, string>())
  const groups = useRef(session.mute_groups)
  groups.current = session.mute_groups
  const commit = (i: number) => {
    const name = drafts.current.get(i)?.trim()
    drafts.current.delete(i)
    if (name && name !== groups.current[i]?.name) act({ cmd: 'rename_mute_group', group: i, name })
  }
  const commitRef = useRef(commit)
  commitRef.current = commit
  useEffect(() => () => [...drafts.current.keys()].forEach((i) => commitRef.current(i)), [])
  return (
    <div className="mg-list">
      <div className="menu-head mg-list-head">
        <span>Mute groups</span>
        <button
          type="button"
          className={`icon-button${naming ? ' on' : ''}`}
          title={naming ? 'Done naming' : 'Name the mute groups'}
          aria-pressed={naming}
          onClick={() => setNaming(!naming)}
        >
          {naming ? <Check size={14} /> : <Pencil size={13} />}
        </button>
      </div>
      {session.mute_groups.map((group, i) => {
        const members = muteGroupMembers(session, i)
        return naming ? (
          <label key={i} className="mg-row naming">
            <span className="mg-num">{i + 1}</span>
            <input
              className="mg-name-edit"
              defaultValue={group.name}
              autoFocus={i === 0}
              aria-label={`Mute group ${i + 1} name`}
              onChange={(e) => drafts.current.set(i, e.currentTarget.value)}
              onBlur={() => commit(i)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') e.currentTarget.blur()
                if (e.key === 'Escape') {
                  // Escape drops the edit (and closes the list).
                  drafts.current.delete(i)
                  e.currentTarget.value = group.name
                }
              }}
            />
          </label>
        ) : (
          <button
            key={i}
            type="button"
            className={`mg-row mg${group.active ? ' on' : ''}${members === 0 ? ' unused' : ''}`}
            aria-pressed={group.active}
            title={muteGroupTitle(session, i)}
            onClick={() => act({ cmd: 'set_mute_group', group: i, active: !group.active })}
          >
            <span className="mg-num">{i + 1}</span>
            <span className="mg-name">{group.name}</span>
            <span className="mg-members value">{members === 0 ? '—' : members}</span>
          </button>
        )
      })}
    </div>
  )
}

const SOLO_MODES: [SoloMode, string, string][] = [
  ['pfl', 'PFL', 'Pre-fader listen: solo goes to the monitor bus only; the PA is untouched'],
  ['afl', 'AFL', 'After-fader listen: solo goes to the monitor bus, after fader and pan; the PA is untouched'],
  ['sip', 'SIP', 'Solo in place: soloing silences every other strip on the main mix — the PA hears it'],
]

/** Solo mode, monitor level and dim, clear solo, and the monitor bus meter. */
export function MonitorControls(props: { session: Session }) {
  const { session } = props
  const phase3 = useStore((s) => s.phase3) === true
  const monitor = session.monitor
  const soloed = soloedCount(session)
  const set = (patch: Partial<Monitor>) => act({ cmd: 'set_monitor', monitor: { ...monitor, ...patch } })
  const sip = monitor.solo_mode === 'sip'
  // PFL/AFL are heard only on the monitor bus: say so when no output takes it.
  const silent = !sip && !monitorPatched(session)
  return (
    <div className={`console-cluster monitor${sip ? ' sip' : ''}`} role="group" aria-label="Monitor">
      <span className="cluster-label" title="Monitor">
        <Headphones size={13} /> <span className="cluster-label-text">Monitor</span>
      </span>
      <div className="segments solo-mode" role="radiogroup" aria-label="Solo mode">
        {SOLO_MODES.map(([mode, label, title]) => (
          <button
            key={mode}
            type="button"
            role="radio"
            aria-checked={monitor.solo_mode === mode}
            className={`${monitor.solo_mode === mode ? 'on' : ''}${mode === 'sip' ? ' sip' : ''}`}
            title={title}
            onClick={() => set({ solo_mode: mode })}
          >
            {label}
          </button>
        ))}
      </div>
      {phase3 && <MonitorSourceSelect session={session} />}
      {silent && (
        <MenuButton
          popover
          popClass="monitor-unpatched-pop"
          className="monitor-unpatched"
          label="Monitor not patched: solo is silent"
          icon={<TriangleAlert size={13} />}
        >
          {(close) => (
            <>
              <p className="pop-note">
                In PFL and AFL a solo is heard only on the monitor bus, and no output takes it, so soloing is silent.
              </p>
              <a className="menu-item" href="#patch/outputs" onClick={close}>
                <span className="menu-icon">
                  <ArrowRight size={14} />
                </span>
                <span className="menu-text">
                  <span className="menu-label">Patch the monitor</span>
                  <span className="menu-detail">Patch → Outputs</span>
                </span>
              </a>
            </>
          )}
        </MenuButton>
      )}
      {sip && (
        <span className="sip-warning" title="Solo in place changes what the audience hears">
          <TriangleAlert size={13} /> <span className="sip-warning-text">Solo affects the PA</span>
        </span>
      )}
      <div className="monitor-level">
        <Knob
          value={dbToPosition(monitor.level_db)}
          min={0}
          max={1}
          defaultValue={dbToPosition(0)}
          size={24}
          hideValue
          label="Monitor level (double-click: 0 dB)"
          format={(p) => formatDb(positionToDb(p))}
          onChange={(p) =>
            actLatest('monitor-level', {
              cmd: 'set_monitor',
              monitor: { ...monitor, level_db: Math.round(positionToDb(p) * 10) / 10 },
            })
          }
        />
        <span className="value monitor-db">{formatDb(monitor.level_db)}</span>
      </div>
      <Latch kind="plain" on={monitor.dim} title="Dim the monitor by 20 dB" onClick={() => set({ dim: !monitor.dim })}>
        DIM
      </Latch>
      <button
        type="button"
        className={`clear-solo${soloed > 0 ? ' on' : ''}`}
        disabled={soloed === 0}
        title={soloed > 0 ? `Clear solo on ${soloed} strip${soloed === 1 ? '' : 's'}` : 'Nothing is soloed'}
        onClick={() => act({ cmd: 'clear_solo' })}
      >
        <span className="clear-solo-s">S</span>
        <span className="value">{soloed}</span>
        <span className="clear-solo-label">Clear</span>
      </button>
      <div className="monitor-meter" title="The monitor bus">
        <LevelBar strip={MONITOR_KEY} side={0} width={48} />
        <LevelBar strip={MONITOR_KEY} side={1} width={48} />
      </div>
    </div>
  )
}

/** What the monitor plays while nothing is soloed: the master, or a bus or
 *  matrix as a cue (to hear a wedge's mix). */
function MonitorSourceSelect(props: { session: Session }) {
  const { session } = props
  const monitor = session.monitor
  const value = stripKey(monitor.source)
  const source = monitor.source
  const exists =
    source.kind === 'master' ||
    (source.kind === 'bus' ? session.buses : session.matrices).some((s) => s.id === source.id)
  const cue = source.kind !== 'master'
  return (
    <Select
      value={value}
      icon={<Ear size={12} />}
      className={`monitor-source${cue ? ' cue' : ''}`}
      title="What the monitor plays while nothing is soloed (a solo still takes over)"
      onChange={(key) => {
        const [kind, id] = key.split(':')
        const source: MonitorSource =
          kind === 'bus' || kind === 'matrix' ? { kind, id: Number(id) } : { kind: 'master' }
        act({ cmd: 'set_monitor', monitor: { ...monitor, source } })
      }}
    >
      <option value="master">Master</option>
      {ROLES.map((role) => {
        const buses = session.buses.filter((b) => b.role === role.role)
        return buses.length === 0 ? null : (
          <optgroup key={role.role} label={role.bank}>
            {buses.map((b) => (
              <option key={b.id} value={`bus:${b.id}`}>
                {b.name}
              </option>
            ))}
          </optgroup>
        )
      })}
      {session.matrices.length > 0 && (
        <optgroup label="Matrix">
          {session.matrices.map((m) => (
            <option key={m.id} value={`matrix:${m.id}`}>
              {m.name}
            </option>
          ))}
        </optgroup>
      )}
      {!exists && <option value={value}>(removed)</option>}
    </Select>
  )
}

// ── DCA faders ──────────────────────────────────────────────────────────

/** A DCA: name, colour, members, its fader and mute, and spill. */
export function DcaStrip(props: {
  index: number
  dca: Dca
  members: number
  spilled: boolean
  onSpill: () => void
  /** Sends on Fader is on: a DCA has no send, so its fader rests. */
  sofBlocked?: string
}) {
  const { index, dca } = props
  const [editing, setEditing] = useState(false)
  const [coloring, setColoring] = useState(false)
  return (
    <div
      className={`strip strip-dca${dca.mute ? ' muted' : ''}${dca.color !== null ? ' colored' : ''}${props.sofBlocked ? ' sof-none' : ''}`}
      style={stripColorStyle(dca.color)}
      title={props.sofBlocked}
    >
      <div className="strip-head">
        <span className="strip-badge">D{index + 1}</span>
        {editing ? (
          <input
            className="strip-name-edit"
            autoFocus
            defaultValue={dca.name}
            onBlur={(e) => {
              setEditing(false)
              const name = e.currentTarget.value.trim()
              if (name && name !== dca.name) act({ cmd: 'rename_dca', dca: index, name })
            }}
            onKeyDown={(e) => {
              if (e.key === 'Enter') e.currentTarget.blur()
              if (e.key === 'Escape') setEditing(false)
            }}
          />
        ) : (
          <span className="strip-name" title="Double-click to rename" onDoubleClick={() => setEditing(true)}>
            {dca.name}
          </span>
        )}
      </div>
      <div className="dca-top">
        <span className={`dca-members${props.members === 0 ? ' none' : ''}`}>
          {props.members === 0 ? 'No strips' : `${props.members} strip${props.members === 1 ? '' : 's'}`}
        </span>
        <button
          type="button"
          className={`dca-color${coloring ? ' on' : ''}`}
          title="Colour"
          aria-expanded={coloring}
          onClick={() => setColoring(!coloring)}
        >
          <span className="dca-color-chip" />
        </button>
        {coloring && (
          <div className="dca-palette">
            <ColorSwatches
              small
              value={dca.color}
              onChange={(color) => {
                act({ cmd: 'set_dca_color', dca: index, color })
                setColoring(false)
              }}
            />
          </div>
        )}
      </div>
      <Fader db={dca.level_db} onChange={(db) => actLatest(`dca:${index}`, { cmd: 'set_dca_level', dca: index, db })} />
      <div className="strip-row latches">
        <Latch
          kind="mute"
          on={dca.mute}
          title="Mute this DCA's strips: their outputs go silent; pre-fader sends and PFL keep going"
          onClick={() => act({ cmd: 'set_dca_mute', dca: index, mute: !dca.mute })}
        >
          M
        </Latch>
      </div>
      <button
        type="button"
        className={`spill${props.spilled ? ' on' : ''}`}
        aria-pressed={props.spilled}
        disabled={props.members === 0 && !props.spilled}
        title={props.members === 0 ? 'Assign strips to this DCA in their Selected Channel' : 'Show only its strips'}
        onClick={props.onSpill}
      >
        <Eye size={13} /> Spill
      </button>
    </div>
  )
}
