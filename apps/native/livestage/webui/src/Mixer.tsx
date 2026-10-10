// The console: the strips of the chosen fader bank (Inputs, Aux, Groups,
// FX, Matrix, DCAs or a custom layer), with the master pinned on the right.
// Laid out like the desktop mixer, top to bottom: name, input, trim, the
// processing section, inserts, sends, pan, fader and meter, DCA and mute
// group membership, mute/solo/arm, output. Everything above and below the
// fader is one fixed-height row (inserts and sends are one-row summaries
// whose racks open in a popover), so the fader takes the rest of the
// strip's height and every strip's fader lines up.
//
// Over it, the console bar: the banks, a DCA's spill, the selection, the
// mute groups and the monitor. A strip's name opens its Selected Channel
// (Shift/Ctrl/⌘-click, or select mode, selects it); its badge drags it to a
// new place; its ⋮ copies, pastes, saves to the library, makes it recall
// safe or removes it.
//
// Sends on Fader: with a bus (or a matrix) chosen, every strip that can feed
// it shows its send level on its fader, re-tinted in that bus's colour, with
// pre/post where the sends summary was and the send's pan (and whether it
// follows) where the pan was. Strips that cannot feed it rest, dimmed.

import { Fragment, memo, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { CSSProperties, PointerEvent as ReactPointerEvent, ReactNode } from 'react'
import {
  ArrowRight,
  ArrowUpRight,
  Check,
  ChevronDown,
  Circle,
  CircleAlert,
  Grid3x3,
  GripVertical,
  LayoutList,
  LoaderCircle,
  Lock,
  Mic,
  Plus,
  Power,
  SlidersVertical,
  Speaker,
  X,
} from 'lucide-react'
import { LayerEditor } from './Banks.tsx'
import {
  AddBusDialog,
  AddMatrixDialog,
  MatrixDialog,
  RoleBadge,
  SofBanner,
  patchedOutputs,
  sofColor,
} from './Bus.tsx'
import { chipColorStyle, ConsoleBar, DcaStrip, dcaMembers, soloHint, stripColorStyle } from './Console.tsx'
import { Fader, GateLight, GrMeter, Knob, Latch, Meter, Select } from './controls.tsx'
import { formatDb, formatPan } from './faderLaw.ts'
import { usePlaybackFed } from './Playback.tsx'
import type {
  BusRole,
  BusStrip,
  ChannelStrip,
  Dca,
  InputPatch,
  InsertSlot,
  InsertState,
  MatrixSource,
  MatrixStrip,
  MuteGroup,
  Processing,
  Session,
  StripCore,
  StripOutput,
  StripRef,
} from './protocol.ts'
import { MAX_FADER_DB, MIN_FADER_DB, stripKey } from './protocol.ts'
import type { Bank, SofTarget } from './routing.ts'
import { MATRIX_SOLO, ROLES, bankRole, bankStrips, matrixSend, roleInfo, sendTo } from './routing.ts'
import { act, actLatest, useStore } from './store.ts'
import { MenuButton, StripMenu } from './Workflow.tsx'
import { pick, picking, useWork } from './workstate.ts'

/** Insert rows always shown, so every rack lines up. */
const RACK_ROWS = 4
/** How far a badge must move before a press becomes a drag. */
const DRAG_START_PX = 5
/** How long a moved strip keeps its new place while the server confirms. */
const ORDER_HOLD_MS = 1500

export interface InsertTarget {
  strip: StripRef
  insert: number
}

type Movable = 'channel' | 'bus' | 'matrix'

interface Reorder {
  kind: Movable
  id: number
  /** How far the strip has followed the pointer, px. */
  dx: number
  /** Where it would land: the gap's x in the scroller's content. */
  line: number | null
}

/** `ids` reordered so `id` lands at `index`. */
function moved(ids: number[], id: number, index: number): number[] {
  const rest = ids.filter((i) => i !== id)
  rest.splice(Math.min(index, rest.length), 0, id)
  return rest
}

/** `strips` in the order of `ids` when both hold the same strips. */
function inOrder<T extends { id: number }>(strips: T[], ids: number[] | undefined): T[] {
  if (!ids || ids.length !== strips.length) return strips
  const byId = new Map(strips.map((s) => [s.id, s]))
  const ordered = ids.map((id) => byId.get(id))
  return ordered.every((s): s is T => s !== undefined) ? ordered : strips
}

const ORDER_KEYS: Record<Movable, 'channels' | 'buses' | 'matrices'> = {
  channel: 'channels',
  bus: 'buses',
  matrix: 'matrices',
}

// ── Sends on Fader ──────────────────────────────────────────────────────

/** What the faders are sending to, shared by every strip view. */
interface SofCtx {
  target: SofTarget
  key: string
  name: string
  color: number | null
  stereo: boolean
  /** A new send's pre/post (the bus role's default). */
  preDefault: boolean
  /** The target, when it is a matrix. */
  matrix: MatrixStrip | null
}

function sofContext(session: Session, target: SofTarget | null): SofCtx | null {
  if (!target) return null
  if (target.kind === 'bus') {
    const bus = session.buses.find((b) => b.id === target.id)
    if (!bus) return null
    return {
      target,
      key: stripKey(target),
      name: bus.name,
      color: bus.color,
      stereo: bus.stereo,
      preDefault: roleInfo(bus.role).preFader,
      matrix: null,
    }
  }
  const matrix = session.matrices.find((m) => m.id === target.id)
  if (!matrix) return null
  return {
    target,
    key: stripKey(target),
    name: matrix.name,
    color: matrix.color,
    stereo: matrix.stereo,
    preDefault: false,
    matrix,
  }
}

/** What a strip shows while Sends on Fader is on. */
type StripSof =
  | {
      mode: 'send'
      key: string
      color: number | null
      to: string
      db: number
      exists: boolean
      /** Null: a matrix contribution (always after the source's fader). */
      pre: boolean | null
      stereo: boolean
      pan: number
      /** Null: a matrix contribution (no follow). */
      panFollow: boolean | null
      /** The strip's own pan, which a following send uses. */
      ownPan: number
      onDb: (db: number) => void
      onPre?: () => void
      onPan?: (pan: number) => void
      onFollow?: () => void
    }
  | { mode: 'none'; color: number | null; to: string; reason: string }
  | { mode: 'target'; color: number | null; to: string }

/** A channel's send to the SoF bus as its fader. */
function channelSof(ctx: SofCtx, channel: ChannelStrip): StripSof {
  if (ctx.target.kind !== 'bus') {
    return { mode: 'none', color: ctx.color, to: ctx.name, reason: 'A channel does not feed a matrix: a matrix takes the master and buses.' }
  }
  const bus = ctx.target.id
  const send = sendTo(channel, bus)
  const pre = send?.pre_fader ?? ctx.preDefault
  const level = send?.level_db ?? MIN_FADER_DB
  const base = { cmd: 'set_send' as const, channel: channel.id, bus }
  return {
    mode: 'send',
    key: ctx.key,
    color: ctx.color,
    to: ctx.name,
    db: level,
    exists: send !== undefined,
    pre,
    stereo: ctx.stereo,
    pan: send?.pan ?? 0,
    panFollow: send?.pan_follow ?? true,
    ownPan: channel.pan,
    // The fader's key is the sends rack's own: one stream per send.
    onDb: (level_db) => actLatest(`send:${channel.id}:${bus}`, { ...base, level_db, pre_fader: pre }),
    onPre: () => act({ ...base, level_db: level, pre_fader: !pre }),
    onPan: send
      ? (pan) => actLatest(`sendpan:${channel.id}:${bus}`, { ...base, level_db: level, pre_fader: pre, pan, pan_follow: false })
      : undefined,
    onFollow: send
      ? () => act({ ...base, level_db: level, pre_fader: pre, pan: send.pan, pan_follow: !send.pan_follow })
      : undefined,
  }
}

/** A bus's or the master's contribution to the SoF matrix as its fader. */
function sourceSof(ctx: SofCtx, source: MatrixSource, ownPan: number): StripSof {
  const matrix = ctx.matrix
  if (!matrix) {
    return { mode: 'none', color: ctx.color, to: ctx.name, reason: 'A bus does not send to another bus.' }
  }
  const send = matrixSend(matrix, source)
  const base = { cmd: 'set_matrix_send' as const, matrix: matrix.id, source }
  const key = `${matrix.id}:${stripKey(source)}`
  return {
    mode: 'send',
    key: ctx.key,
    color: ctx.color,
    to: ctx.name,
    db: send.level_db,
    exists: send.level_db > MIN_FADER_DB,
    pre: null,
    stereo: ctx.stereo,
    pan: send.pan,
    panFollow: null,
    ownPan,
    onDb: (level_db) => actLatest(`msend:${key}`, { ...base, level_db, pan: send.pan }),
    onPan: (pan) => actLatest(`msendpan:${key}`, { ...base, level_db: send.level_db, pan }),
  }
}

// ── The mixer ───────────────────────────────────────────────────────────

type Adding = { kind: 'bus'; role: BusRole } | { kind: 'matrix' } | null

export function Mixer(props: {
  session: Session
  inputs: number
  onOpenInsert: (target: InsertTarget) => void
  onAddEffect: (strip: StripRef) => void
  onSelect: (strip: StripRef) => void
  bank: Bank
  onBank: (bank: Bank) => void
  spill: number | null
  onSpill: (dca: number | null) => void
  sof: SofTarget | null
  onSof: (target: SofTarget | null) => void
}) {
  const { session, spill, bank } = props
  const insertStates = useStore((s) => s.insertStates)
  const phase3 = useStore((s) => s.phase3)
  // The highest "Ch N" asked for and not yet in the session.
  const asked = useRef(0)
  const scroller = useRef<HTMLDivElement>(null)
  const [reorder, setReorder] = useState<Reorder | null>(null)
  const [adding, setAdding] = useState<Adding>(null)
  const [editingLayer, setEditingLayer] = useState<number | null>(null)
  const [matrixOpen, setMatrixOpen] = useState<number | null>(null)
  // A strip just dropped keeps its new place until the session agrees.
  const [order, setOrder] = useState<{ channels?: number[]; buses?: number[]; matrices?: number[]; at: number } | null>(
    null,
  )
  useEffect(() => {
    if (!order) return
    const agrees = (strips: { id: number }[], ids?: number[]) => !ids || strips.map((s) => s.id).join() === ids.join()
    if (
      agrees(session.channels, order.channels) &&
      agrees(session.buses, order.buses) &&
      agrees(session.matrices, order.matrices)
    ) {
      setOrder(null)
      return
    }
    const timer = window.setTimeout(() => setOrder(null), Math.max(0, order.at + ORDER_HOLD_MS - performance.now()))
    return () => window.clearTimeout(timer)
  }, [order, session.channels, session.buses, session.matrices])

  const allChannels = inOrder(session.channels, order?.channels)
  const allBuses = inOrder(session.buses, order?.buses)
  const allMatrices = inOrder(session.matrices, order?.matrices)
  const spilling = spill !== null && bank !== 'dcas'
  const custom = typeof bank !== 'string'

  // The strips this bank shows, in order.
  let shown: StripRef[]
  if (spilling) {
    shown = [
      ...allChannels.filter((c) => c.dcas.includes(spill)).map((c): StripRef => ({ kind: 'channel', id: c.id })),
      ...allBuses.filter((b) => b.dcas.includes(spill)).map((b): StripRef => ({ kind: 'bus', id: b.id })),
    ]
  } else if (custom) {
    shown = bankStrips(session, bank)
  } else if (bank === 'inputs') {
    shown = allChannels.map((c) => ({ kind: 'channel', id: c.id }))
  } else if (bank === 'matrix') {
    shown = allMatrices.map((m) => ({ kind: 'matrix', id: m.id }))
  } else {
    const role = bankRole(bank)
    shown = role ? allBuses.filter((b) => b.role === role).map((b) => ({ kind: 'bus', id: b.id })) : []
  }

  const sofTarget = props.sof
  const sofMatrix = sofTarget?.kind === 'matrix' ? session.matrices.find((m) => m.id === sofTarget.id) : undefined
  const sofBus = sofTarget?.kind === 'bus' ? session.buses.find((b) => b.id === sofTarget.id) : undefined
  // Rebuilt only when the target (or the matrix it reads) changes, so a
  // memoised strip stays put otherwise.
  const sof = useMemo(
    () => sofContext(session, sofTarget),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [sofTarget?.kind, sofTarget?.id, sofMatrix, sofBus],
  )

  const startReorder = (e: ReactPointerEvent<HTMLElement>, kind: Movable, id: number, strip: StripRef) => {
    const box = scroller.current
    if (e.button !== 0 || !box) return
    e.preventDefault()
    const handle = e.currentTarget
    handle.setPointerCapture(e.pointerId)
    const startX = e.clientX
    const startScroll = box.scrollLeft
    let active = false
    let gap: { before: number | null; after: number | null } | null = null
    const others = () =>
      [...box.querySelectorAll<HTMLElement>(`[data-reorder="${kind}"]`)].filter((el) => Number(el.dataset.id) !== id)
    const onMove = (ev: PointerEvent) => {
      if (!active && Math.abs(ev.clientX - startX) < DRAG_START_PX) return
      active = true
      const bounds = box.getBoundingClientRect()
      // Near an edge, the row scrolls under the strip.
      if (ev.clientX < bounds.left + 48) box.scrollLeft -= 14
      else if (ev.clientX > bounds.right - 48) box.scrollLeft += 14
      const list = others()
      const slot = list.filter((el) => {
        const r = el.getBoundingClientRect()
        return ev.clientX > r.left + r.width / 2
      }).length
      const toContent = (x: number) => x - bounds.left + box.scrollLeft
      let line: number | null = null
      if (list.length > 0) {
        line =
          slot < list.length
            ? toContent(list[slot].getBoundingClientRect().left) - 4
            : toContent(list[list.length - 1].getBoundingClientRect().right) + 3
      }
      gap = {
        before: slot < list.length ? Number(list[slot].dataset.id) : null,
        after: slot > 0 ? Number(list[slot - 1].dataset.id) : null,
      }
      setReorder({ kind, id, dx: ev.clientX - startX + (box.scrollLeft - startScroll), line })
    }
    const onUp = (ev: PointerEvent) => {
      handle.removeEventListener('pointermove', onMove)
      handle.removeEventListener('pointerup', onUp)
      handle.removeEventListener('pointercancel', onCancel)
      setReorder(null)
      if (!active) {
        // A tap on the badge opens the strip, as a console's SEL key; with a
        // modifier (or in select mode) it selects it.
        if (picking(ev)) onPick(strip, ev.shiftKey)
        else props.onSelect(strip)
        return
      }
      if (!gap) return
      // Placed among the strips shown (a bank's or a spill's): its index in
      // the whole list is just before the strip it was dropped ahead of.
      const all = kind === 'channel' ? allChannels : kind === 'bus' ? allBuses : allMatrices
      const ids = all.map((s) => s.id)
      const rest = ids.filter((i) => i !== id)
      const index =
        gap.before !== null ? rest.indexOf(gap.before) : gap.after !== null ? rest.indexOf(gap.after) + 1 : rest.length
      if (index < 0 || ids.indexOf(id) === index) return
      const next = moved(ids, id, index)
      setOrder((o) => ({ ...o, [ORDER_KEYS[kind]]: next, at: performance.now() }))
      if (kind === 'channel') act({ cmd: 'move_channel', channel: id, index })
      else if (kind === 'bus') act({ cmd: 'move_bus', bus: id, index })
      else act({ cmd: 'move_matrix', matrix: id, index })
    }
    const onCancel = () => {
      active = false
      gap = null
      handle.removeEventListener('pointermove', onMove)
      handle.removeEventListener('pointerup', onUp)
      handle.removeEventListener('pointercancel', onCancel)
      setReorder(null)
    }
    handle.addEventListener('pointermove', onMove)
    handle.addEventListener('pointerup', onUp)
    handle.addEventListener('pointercancel', onCancel)
  }

  // One handler for every strip's badge, so a memoised strip stays put.
  const reorderRef = useRef(startReorder)
  reorderRef.current = startReorder
  const onReorder = useCallback<Common['onReorder']>((...args) => reorderRef.current(...args), [])
  // Shift-click extends through the strips shown, in their order here.
  const shownRef = useRef<StripRef[]>([])
  shownRef.current = bank === 'dcas' ? [] : [...shown, { kind: 'master' }]
  const onPick = useCallback<Common['onPick']>((strip, range) => pick(strip, range, shownRef.current), [])
  const onSofRef = useRef(props.onSof)
  onSofRef.current = props.onSof
  const onSof = useCallback<Common['onSof']>((target) => onSofRef.current(target), [])
  const onOpenMatrix = useCallback((id: number) => setMatrixOpen(id), [])

  const common: Common = {
    buses: session.buses,
    inputs: props.inputs,
    insertStates,
    dcas: session.dcas,
    muteGroups: session.mute_groups,
    soloHint: soloHint(session),
    phase3: phase3 !== false,
    sof,
    onOpenInsert: props.onOpenInsert,
    onAddEffect: props.onAddEffect,
    onSelect: props.onSelect,
    onPick,
    onReorder,
    onSof,
  }

  const addChannel = () => {
    const number = Math.max(session.channels.length, asked.current) + 1
    asked.current = number
    // The next input nobody has yet, if there is one.
    const used = new Set(session.channels.flatMap((c) => [c.input.left, c.input.right]))
    let next: number | null = null
    for (let i = 0; i < props.inputs; i++) {
      if (!used.has(i)) {
        next = i
        break
      }
    }
    act({ cmd: 'add_channel', name: `Ch ${number}`, input: { left: next, right: null } })
  }

  const dragging = (kind: Movable, id: number) => (reorder && reorder.kind === kind && reorder.id === id ? reorder.dx : null)
  // In a custom layer the order is the layer's (its editor sets it).
  const reorderable = !custom

  const renderStrip = (ref: StripRef) => {
    if (ref.kind === 'channel') {
      const channel = allChannels.find((c) => c.id === ref.id)
      if (!channel) return null
      return (
        <ChannelView
          channel={channel}
          number={allChannels.indexOf(channel) + 1}
          dragX={dragging('channel', channel.id)}
          movable={reorderable}
          {...common}
        />
      )
    }
    if (ref.kind === 'bus') {
      const bus = allBuses.find((b) => b.id === ref.id)
      if (!bus) return null
      return (
        <BusView
          bus={bus}
          dragX={dragging('bus', bus.id)}
          movable={reorderable}
          feeds={
            session.channels.filter(
              (c) => (c.output.kind === 'bus' && c.output.id === bus.id) || c.sends.some((s) => s.bus === bus.id),
            ).length
          }
          {...common}
        />
      )
    }
    if (ref.kind === 'matrix') {
      const matrix = allMatrices.find((m) => m.id === ref.id)
      if (!matrix) return null
      return (
        <MatrixView
          matrix={matrix}
          dragX={dragging('matrix', matrix.id)}
          movable={reorderable}
          sourceNames={sourceNames(session)}
          patched={patchedOutputs(session, ref)}
          onOpenMatrix={onOpenMatrix}
          {...common}
        />
      )
    }
    return null
  }

  const role = bankRole(bank)
  let addTile: ReactNode = null
  let empty: ReactNode = null
  if (!spilling) {
    if (bank === 'inputs') {
      addTile = <AddTile label="Channel" onClick={addChannel} />
      if (shown.length === 0) empty = 'No channels yet. Add one for each input you mix.'
    } else if (role) {
      const info = roleInfo(role)
      addTile = <AddTile label={`${info.label} bus`} onClick={() => setAdding({ kind: 'bus', role })} />
      if (shown.length === 0) {
        empty =
          role === 'aux'
            ? 'No monitor mixes yet. Add an aux for each wedge or in-ear mix: its sends are pre-fader.'
            : role === 'group'
              ? 'No subgroups yet. A group sums channels routed to it and feeds the master.'
              : 'No FX buses yet. Add one and put a reverb or delay in its inserts: its sends are post-fader.'
      }
    } else if (bank === 'matrix') {
      addTile =
        phase3 === false ? null : <AddTile label="Matrix" onClick={() => setAdding({ kind: 'matrix' })} />
      if (shown.length === 0) {
        empty =
          phase3 === false
            ? 'This LiveStage server predates matrices. Update the server to add one.'
            : 'No matrices yet. A matrix feeds a delay zone, a fill or a record feed from the master and buses.'
      }
    } else if (custom) {
      addTile = (
        <AddTile label="Edit layer" icon={<LayoutList size={16} />} onClick={() => setEditingLayer(bank.layer)} />
      )
      if (shown.length === 0) empty = 'This layer is empty. Edit it to choose its strips.'
    }
  }

  const openMatrix = matrixOpen !== null ? session.matrices.find((m) => m.id === matrixOpen) : undefined
  const masterSof: StripSof | undefined =
    sof && sof.target.kind === 'matrix' ? sourceSof(sof, { kind: 'master' }, session.master.pan) : undefined

  return (
    <div className="mixer">
      <ConsoleBar session={session} bank={bank} onBank={props.onBank} spill={spill} onSpill={props.onSpill} />
      {sof && <SofBanner session={session} target={sof.target} onExit={() => props.onSof(null)} />}
      <div className="mixer-main">
        <div className={`mixer-scroll${reorder ? ' reordering' : ''}`} ref={scroller}>
          {bank === 'dcas' ? (
            session.dcas.map((dca, index) => (
              <DcaStrip
                key={index}
                index={index}
                dca={dca}
                members={dcaMembers(session, index).length}
                spilled={spill === index}
                sofBlocked={sof ? `A DCA has no send to ${sof.name}: its fader rests while Sends on Fader is on` : undefined}
                onSpill={() => {
                  // Choosing a bank clears a spill, so the bank goes first.
                  if (spill === index) {
                    props.onSpill(null)
                    return
                  }
                  props.onBank('inputs')
                  props.onSpill(index)
                }}
              />
            ))
          ) : (
            <>
              {shown.map((ref, i) => (
                <Fragment key={stripKey(ref)}>
                  {i > 0 && shown[i - 1].kind !== ref.kind && <div className="mixer-divider" />}
                  {renderStrip(ref)}
                </Fragment>
              ))}
              {empty && <div className="spill-empty bank-empty">{empty}</div>}
              {addTile}
              {spilling && shown.length === 0 && (
                <div className="spill-empty">
                  No strips follow {session.dcas[spill].name}. Assign them in a strip's Selected Channel.
                </div>
              )}
            </>
          )}
          {reorder && reorder.line !== null && <div className="reorder-line" style={{ left: reorder.line }} />}
        </div>
        <div className="mixer-master">
          <StripFrame
            {...common}
            strip={{ kind: 'master' }}
            badge={<Speaker size={12} />}
            name="Master"
            core={session.master}
            recordArm={session.master.record_arm}
            top={<div className="strip-note">Main mix</div>}
            stripSof={masterSof}
          />
        </div>
      </div>
      {adding?.kind === 'bus' && (
        <AddBusDialog
          session={session}
          role={adding.role}
          onClose={() => setAdding(null)}
          onAdded={(r) => {
            const target = r === 'aux' ? 'aux' : r === 'group' ? 'groups' : 'fx'
            if (!custom && bank !== target) props.onBank(target)
          }}
        />
      )}
      {adding?.kind === 'matrix' && <AddMatrixDialog session={session} onClose={() => setAdding(null)} />}
      {editingLayer !== null && (
        <LayerEditor
          session={session}
          index={editingLayer}
          layer={session.layers[editingLayer] ?? null}
          onClose={() => setEditingLayer(null)}
          onSaved={(index) => props.onBank({ layer: index })}
          onRemoved={() => props.onBank('inputs')}
        />
      )}
      {openMatrix && (
        <MatrixDialog session={session} matrix={openMatrix} onClose={() => setMatrixOpen(null)} onSof={props.onSof} />
      )}
    </div>
  )
}

/** Each matrix source's name, by key, for the matrix strips' summaries. */
function sourceNames(session: Session): Map<string, string> {
  return new Map([['master', 'Master'], ...session.buses.map((b): [string, string] => [`bus:${b.id}`, b.name])])
}

function AddTile(props: { label: string; onClick: () => void; icon?: ReactNode }) {
  return (
    <button type="button" className="add-tile" onClick={props.onClick} title={`Add: ${props.label.toLowerCase()}`}>
      <span className="add-tile-icon">{props.icon ?? <Plus size={16} />}</span>
      <span className="add-tile-label">{props.label}</span>
    </button>
  )
}

interface Common {
  buses: BusStrip[]
  inputs: number
  insertStates: Record<string, InsertState>
  dcas: Dca[]
  muteGroups: MuteGroup[]
  /** Where a solo is heard now (solo_mode, monitor patch). */
  soloHint: string
  /** The server has Phase 3 (or has not said yet). */
  phase3: boolean
  /** Sends on Fader, when on. */
  sof: SofCtx | null
  onOpenInsert: (target: InsertTarget) => void
  onAddEffect: (strip: StripRef) => void
  onSelect: (strip: StripRef) => void
  /** A modified click (or a tap in select mode) on the strip's header:
   *  toggle it in the selection, or extend the selection to it. */
  onPick: (strip: StripRef, range: boolean) => void
  onReorder: (e: ReactPointerEvent<HTMLElement>, kind: Movable, id: number, strip: StripRef) => void
  onSof: (target: SofTarget | null) => void
}

const ChannelView = memo(function ChannelView(
  props: Common & { channel: ChannelStrip; number: number; dragX: number | null; movable: boolean },
) {
  const { channel } = props
  return (
    <StripFrame
      {...props}
      strip={{ kind: 'channel', id: channel.id }}
      badge={props.number}
      name={channel.name}
      core={channel}
      recordArm={channel.record_arm}
      top={
        <>
          <div className="strip-input">
            <InputSelect channel={channel} inputs={props.inputs} />
            <PlaybackInput channel={channel.id} />
          </div>
          <div className="strip-row trim-row">
            <Knob
              value={channel.trim_db}
              min={-24}
              max={24}
              defaultValue={0}
              bipolar
              size={24}
              label="Trim (double-click: 0 dB)"
              hideValue
              format={(v) => `${v > 0 ? '+' : ''}${v.toFixed(1)}`}
              onChange={(db) => actLatest(`trim:${channel.id}`, { cmd: 'set_trim', channel: channel.id, db })}
            />
            <div className="trim-text">
              <span className="caption">Trim</span>
              <span className="value">
                {channel.trim_db > 0 ? '+' : ''}
                {channel.trim_db.toFixed(1)}
              </span>
            </div>
            <Latch
              kind="plain"
              on={channel.phase_invert}
              title="Polarity invert"
              onClick={() => act({ cmd: 'set_phase_invert', channel: channel.id, invert: !channel.phase_invert })}
            >
              Ø
            </Latch>
          </div>
        </>
      }
      middle={<SendSummary channel={channel} buses={props.buses} />}
      output={channel.output}
      meterTap
      stripSof={props.sof ? channelSof(props.sof, channel) : undefined}
    />
  )
})

/** Virtual soundcheck: this channel's input (and its input meter) is the
 *  take's track, not the interface; drawn over the input select's end so
 *  the strip keeps its height. */
function PlaybackInput(props: { channel: number }) {
  const file = usePlaybackFed(props.channel)
  if (!file) return null
  return (
    <a
      className="pb-chip strip-pb"
      href="#patch/playback"
      title={`Virtual soundcheck: this channel hears ${file} from the take, not its interface input (the input meter shows the playback)`}
    >
      PB
    </a>
  )
}

/** A bus's role and width, and its Sends on Fader key. */
function BusTop(props: { bus: BusStrip; sofOn: boolean; phase3: boolean; onSof: (target: SofTarget | null) => void }) {
  const { bus } = props
  const old = !props.phase3
  return (
    <div className="bus-top">
      <div className="bus-top-row">
        <Select
          value={bus.role}
          className="role-select"
          title={old ? 'This LiveStage server predates bus roles' : `Role: ${roleInfo(bus.role).detail}`}
          onChange={(role) => act({ cmd: 'set_bus_role', bus: bus.id, role: role as BusRole })}
        >
          {ROLES.map((r) => (
            <option key={r.role} value={r.role}>
              {r.label}
            </option>
          ))}
        </Select>
        <button
          type="button"
          className={`width-toggle${bus.stereo ? '' : ' mono'}`}
          title={
            old
              ? 'This LiveStage server predates mono buses'
              : bus.stereo
                ? 'Stereo bus: click to make it mono (sends lose their pan)'
                : 'Mono bus: click to make it stereo'
          }
          onClick={() => act({ cmd: 'set_bus_stereo', bus: bus.id, stereo: !bus.stereo })}
        >
          {bus.stereo ? 'ST' : 'MONO'}
        </button>
      </div>
      <SofKey
        on={props.sofOn}
        color={bus.color}
        name={bus.name}
        onClick={() => props.onSof(props.sofOn ? null : { kind: 'bus', id: bus.id })}
      />
    </div>
  )
}

/** The key that puts a bus's (or matrix's) sends on the faders. */
function SofKey(props: { on: boolean; color: number | null; name: string; onClick: () => void }) {
  return (
    <button
      type="button"
      className={`sof-key${props.on ? ' on' : ''}`}
      style={{ '--sof-color': sofColor(props.color) } as CSSProperties}
      aria-pressed={props.on}
      title={props.on ? 'Back to the strips’ own faders (Esc)' : `Sends on Fader: the faders become the sends to ${props.name}`}
      onClick={props.onClick}
    >
      <SlidersVertical size={12} />
      <span>{props.on ? 'Exit SoF' : 'Sends on fader'}</span>
    </button>
  )
}

const BusView = memo(function BusView(
  props: Common & { bus: BusStrip; feeds: number; dragX: number | null; movable: boolean },
) {
  const { bus, sof } = props
  const info = roleInfo(bus.role)
  const isTarget = sof?.target.kind === 'bus' && sof.target.id === bus.id
  let stripSof: StripSof | undefined
  if (sof) {
    stripSof = isTarget
      ? { mode: 'target', color: sof.color, to: sof.name }
      : sof.target.kind === 'matrix'
        ? sourceSof(sof, { kind: 'bus', id: bus.id }, bus.pan)
        : { mode: 'none', color: sof.color, to: sof.name, reason: 'A bus does not send to another bus.' }
  }
  return (
    <StripFrame
      {...props}
      strip={{ kind: 'bus', id: bus.id }}
      badge={<RoleBadge role={bus.role} />}
      name={bus.name}
      core={bus}
      recordArm={bus.record_arm}
      top={<BusTop bus={bus} sofOn={isTarget} phase3={props.phase3} onSof={props.onSof} />}
      middle={
        <div
          className="sum-row bus-info"
          title={[
            bus.role === 'aux' ? 'Monitor mix' : bus.role === 'group' ? 'Subgroup' : 'Effect send + return',
            `Sends ${info.preFader ? 'pre' : 'post'}-fader`,
            bus.stereo ? 'Stereo' : 'Mono: L+R on both sides',
            props.feeds === 0 ? 'Nothing feeds it yet' : `Fed by ${props.feeds} channel${props.feeds === 1 ? '' : 's'}`,
          ].join(' · ')}
        >
          <span className="bus-info-role">{info.preFader ? 'Pre' : 'Post'}</span>
          <span className="bus-info-feeds">
            {props.feeds === 0 ? 'not fed' : `fed by ${props.feeds}`}
          </span>
        </div>
      }
      output={bus.output}
      isBus
      stripSof={stripSof}
    />
  )
})

const MatrixView = memo(function MatrixView(
  props: Common & {
    matrix: MatrixStrip
    dragX: number | null
    movable: boolean
    sourceNames: Map<string, string>
    patched: string
    onOpenMatrix: (id: number) => void
  },
) {
  const { matrix, sof } = props
  const isTarget = sof?.target.kind === 'matrix' && sof.target.id === matrix.id
  const live = matrix.sources.filter((s) => s.level_db > MIN_FADER_DB)
  let stripSof: StripSof | undefined
  if (sof) {
    stripSof = isTarget
      ? { mode: 'target', color: sof.color, to: sof.name }
      : { mode: 'none', color: sof.color, to: sof.name, reason: 'A matrix feeds nothing else: it goes to its outputs only.' }
  }
  return (
    <StripFrame
      {...props}
      strip={{ kind: 'matrix', id: matrix.id }}
      badge={<Grid3x3 size={11} />}
      name={matrix.name}
      core={matrix}
      recordArm={matrix.record_arm}
      top={
        <div className="bus-top">
          <div className="bus-top-row">
            <button
              type="button"
              className="matrix-sources-button"
              title={`${matrix.name}'s sources: the master and buses, each at its own level`}
              onClick={() => props.onOpenMatrix(matrix.id)}
            >
              <Grid3x3 size={12} /> {live.length} source{live.length === 1 ? '' : 's'}
            </button>
            <button
              type="button"
              className={`width-toggle${matrix.stereo ? '' : ' mono'}`}
              title={matrix.stereo ? 'Stereo matrix: click to make it mono' : 'Mono matrix: click to make it stereo'}
              onClick={() => act({ cmd: 'set_matrix_stereo', matrix: matrix.id, stereo: !matrix.stereo })}
            >
              {matrix.stereo ? 'ST' : 'MONO'}
            </button>
          </div>
          <SofKey
            on={isTarget}
            color={matrix.color}
            name={matrix.name}
            onClick={() => props.onSof(isTarget ? null : { kind: 'matrix', id: matrix.id })}
          />
        </div>
      }
      middle={
        <button
          type="button"
          className={`sum-row matrix-sum${live.length === 0 ? ' empty' : ''}`}
          title={
            live.length === 0
              ? 'Silent: no sources yet. Open the sources: level and pan per source'
              : `${live
                  .map((s) => `${props.sourceNames.get(stripKey(s.source)) ?? '?'} ${formatDb(s.level_db)}`)
                  .join(', ')}. Open the sources: level and pan per source`
          }
          onClick={() => props.onOpenMatrix(matrix.id)}
        >
          {live.length === 0 ? (
            <span className="matrix-src-name">Silent: no sources</span>
          ) : (
            <>
              <span className="matrix-src-name">{props.sourceNames.get(stripKey(live[0].source)) ?? '?'}</span>
              <span className="value">{formatDb(live[0].level_db)}</span>
              {live.length > 1 && <span className="sum-more-count">+{live.length - 1}</span>}
            </>
          )}
        </button>
      }
      outputNote={
        <a
          className={`strip-output-label${props.patched ? '' : ' unpatched'}`}
          href="#patch/outputs"
          title="A matrix is heard only where Patch → Outputs sends it"
        >
          <Speaker size={12} /> {props.patched || 'Not patched'}
        </a>
      }
      stripSof={stripSof}
    />
  )
})

/** Why a strip is silent though its own mute is off: a muted DCA or an
 *  active mute group it follows (null when nothing mutes it). */
function impliedMute(core: StripCore, dcas: Dca[], groups: MuteGroup[]): string | null {
  const by = [
    ...core.dcas.filter((d) => dcas[d]?.mute).map((d) => dcas[d].name),
    ...core.mute_groups.filter((g) => groups[g]?.active).map((g) => groups[g].name),
  ]
  return by.length > 0 ? `Muted by ${by.join(', ')}` : null
}

function StripFrame(
  props: Common & {
    strip: StripRef
    badge: ReactNode
    name: string
    core: StripCore
    recordArm: boolean
    top?: ReactNode
    middle?: ReactNode
    output?: StripOutput
    /** In place of the output select (a matrix's patch). */
    outputNote?: ReactNode
    isBus?: boolean
    meterTap?: boolean
    dragX?: number | null
    /** Whether the badge drags the strip (not in a custom layer). */
    movable?: boolean
    stripSof?: StripSof
  },
) {
  const { strip, core, stripSof: sof } = props
  const key = stripKey(strip)
  const [showInput, setShowInput] = useState(false)
  const picked = useWork((w) => w.selection.includes(key))
  const implied = core.mute ? null : impliedMute(core, props.dcas, props.muteGroups)
  const movable = strip.kind !== 'master' && props.movable !== false
  const dragged = props.dragX !== null && props.dragX !== undefined
  // A matrix takes mute groups (never a DCA): its chips show them.
  const assignable = strip.kind !== 'master'
  const sending = sof?.mode === 'send' ? sof : null
  const sofClass = sof ? ` sof-${sof.mode}` : ''
  const style: CSSProperties = {
    ...stripColorStyle(core.color),
    ...(sof ? ({ '--sof-color': sofColor(sof.color) } as CSSProperties) : null),
    ...(dragged ? { transform: `translateX(${props.dragX}px)` } : null),
  }
  let middle = props.middle
  if (sending) middle = <SofSend sof={sending} />
  else if (sof?.mode === 'none') {
    middle = (
      <div className="sum-row sof-note" title={sof.reason}>
        No send to {sof.to}
      </div>
    )
  } else if (sof?.mode === 'target') {
    middle = (
      <div className="sum-row sof-note target" title={`This is ${sof.to}: its own level. The other faders send to it.`}>
        {sof.to}'s own level
      </div>
    )
  }
  return (
    <div
      className={`strip strip-${strip.kind}${core.mute || implied ? ' muted' : ''}${core.color !== null ? ' colored' : ''}${dragged ? ' dragged' : ''}${picked ? ' picked' : ''}${sofClass}`}
      style={style}
      data-reorder={movable ? strip.kind : undefined}
      data-id={strip.kind !== 'master' ? strip.id : undefined}
    >
      <StripName
        strip={strip}
        name={props.name}
        badge={props.badge}
        picked={picked}
        recallSafe={core.recall_safe}
        onSelect={() => props.onSelect(strip)}
        onPick={props.onPick}
        onHandle={movable ? (e) => props.onReorder(e, strip.kind as Movable, strip.id, strip) : undefined}
      />
      <div className="strip-top">{props.top}</div>
      <ProcessingRow strip={strip} processing={core.processing} onSelect={() => props.onSelect(strip)} />
      <InsertSummary
        inserts={core.inserts}
        states={props.insertStates}
        onOpen={(insert) => props.onOpenInsert({ strip, insert })}
        onAdd={() => props.onAddEffect(strip)}
      />
      <div className="strip-middle">{middle}</div>
      <div className="strip-row pan-row">
        {sending ? (
          <SofPan sof={sending} />
        ) : (
          <Knob
            value={core.pan}
            min={-1}
            max={1}
            defaultValue={0}
            bipolar
            size={26}
            caption={strip.kind === 'channel' ? 'Pan' : 'Bal'}
            label={`${strip.kind === 'channel' ? 'Pan' : 'Balance'} (double-click: centre)`}
            format={formatPan}
            onChange={(pan) => actLatest(`pan:${key}`, { cmd: 'set_pan', strip, pan })}
          />
        )}
      </div>
      {sending ? (
        <Fader
          key={`sof:${sending.key}`}
          db={sending.db}
          onChange={sending.onDb}
          meter={
            <div className="strip-meters">
              <Meter strip={key} />
            </div>
          }
        />
      ) : (
        <Fader
          key="own"
          db={core.fader_db}
          onChange={(db) => actLatest(`fader:${key}`, { cmd: 'set_fader', strip, db })}
          meter={
            <div className="strip-meters">
              {props.meterTap && showInput && <Meter strip={key} tap="input" />}
              <Meter strip={key} />
              <GrMeter strip={key} comp={core.processing.comp.on} gate={core.processing.gate.on} />
            </div>
          }
        />
      )}
      {assignable && <Membership core={core} dcas={props.dcas} groups={props.muteGroups} />}
      <div className="strip-row latches">
        <Latch
          kind="mute"
          on={core.mute}
          implied={implied !== null}
          title={`${implied ?? 'Mute'}: the strip's output goes silent; pre-fader sends and PFL keep going`}
          onClick={() => act({ cmd: 'set_mute', strip, mute: !core.mute })}
        >
          M
        </Latch>
        {strip.kind !== 'master' && (
          <Latch
            kind="solo"
            on={core.solo}
            title={
              strip.kind === 'matrix'
                ? MATRIX_SOLO
                : `Solo — ${props.soloHint}${core.solo_safe ? '. Solo safe: never silenced by solo in place' : ''}`
            }
            onClick={() => act({ cmd: 'set_solo', strip, solo: !core.solo })}
          >
            S
          </Latch>
        )}
        <Latch
          kind="arm"
          on={props.recordArm}
          title="Record this strip"
          onClick={() => act({ cmd: 'set_record_arm', strip, arm: !props.recordArm })}
        >
          <Circle size={10} fill="currentColor" strokeWidth={0} />
        </Latch>
        {props.meterTap && !sending && (
          <Latch kind="plain" on={showInput} title="Show the input meter" onClick={() => setShowInput(!showInput)}>
            IN
          </Latch>
        )}
      </div>
      {props.output ? (
        <OutputSelect strip={strip} output={props.output} buses={props.isBus ? [] : props.buses} />
      ) : (
        (props.outputNote ?? (
          <div className="strip-output-label">
            <Speaker size={12} /> Main out
          </div>
        ))
      )}
    </div>
  )
}

/** Where the sends rack was, in Sends on Fader: what this fader sends to,
 *  pre/post, and whether its pan follows the strip's. */
function SofSend(props: { sof: Extract<StripSof, { mode: 'send' }> }) {
  const { sof } = props
  const state = sof.exists
    ? `This fader is the send to ${sof.to}: ${formatDb(sof.db)}`
    : `No send to ${sof.to} yet: raise the fader to make one${sof.pre === null ? '' : ` (${sof.pre ? 'pre' : 'post'})`}`
  return (
    <div className={`sum-row sof-send${sof.exists ? '' : ' unsent'}`} title={state}>
      {sof.pre !== null ? (
        <div className="segments sof-prepost" role="radiogroup" aria-label="Pre or post fader">
          <button
            type="button"
            role="radio"
            aria-checked={sof.pre}
            className={sof.pre ? 'on' : ''}
            title="Pre-fader: the strip's own fader does not change the send (monitor mixes)"
            onClick={() => !sof.pre && sof.onPre?.()}
          >
            PRE
          </button>
          <button
            type="button"
            role="radio"
            aria-checked={!sof.pre}
            className={!sof.pre ? 'on' : ''}
            title="Post-fader: the send follows the strip's fader (effect sends)"
            onClick={() => sof.pre && sof.onPre?.()}
          >
            POST
          </button>
        </div>
      ) : (
        <span className="sof-send-hint">After its fader</span>
      )}
      {!sof.exists && <span className="sof-off">off</span>}
    </div>
  )
}

/** The send's pan, in place of the strip's own in Sends on Fader, and
 *  whether it follows the channel's. */
function SofPan(props: { sof: Extract<StripSof, { mode: 'send' }> }) {
  const { sof } = props
  if (!sof.stereo) return <span className="sof-mono" title={`${sof.to} is mono: its sends have no pan`}>Mono · no pan</span>
  const follow =
    sof.panFollow !== null ? (
      <button
        type="button"
        className={`pill sof-follow${sof.panFollow ? ' on' : ''}`}
        disabled={!sof.onFollow}
        aria-pressed={sof.panFollow}
        title={
          !sof.onFollow
            ? 'Raise the fader to make the send first'
            : sof.panFollow
              ? "The send's pan follows the channel's pan. Click to give it its own"
              : "The send has its own pan. Click to follow the channel's pan"
        }
        onClick={() => sof.onFollow?.()}
      >
        {sof.panFollow ? 'FOLLOW' : 'OWN'}
      </button>
    ) : null
  if (!sof.onPan) {
    return (
      <div className="sof-pan">
        <span className="sof-mono">Pan once sent</span>
        {follow}
      </div>
    )
  }
  const following = sof.panFollow === true
  // A pre-fader send that follows is taken before the channel's pan:
  // unpanned (as sends always were).
  const unpanned = following && sof.pre === true
  const value = unpanned ? 0 : following ? sof.ownPan : sof.pan
  return (
    <div className={`sof-pan${following ? ' following' : ''}`}>
      <Knob
        value={value}
        min={-1}
        max={1}
        defaultValue={0}
        bipolar
        size={26}
        caption={unpanned ? 'pre' : following ? 'follows' : 'send'}
        label={
          unpanned
            ? "Send pan: a pre-fader send that follows the channel is taken before its pan, so it is unpanned. Turn it to give the send its own"
            : following
              ? "Send pan: follows the channel's pan. Turn it to give the send its own"
              : `Send pan into ${sof.to} (double-click: centre)`
        }
        format={formatPan}
        onChange={(pan) => sof.onPan?.(pan)}
      />
      {follow}
    </div>
  )
}

/** The processing section at a glance: which parts are in, the gate's open
 *  light. Opens the Selected Channel. */
function ProcessingRow(props: { strip: StripRef; processing: Processing; onSelect: () => void }) {
  const p = props.processing
  const eqActive = p.eq.on && p.eq.bands.some((b) => Math.abs(b.gain_db) >= 0.05)
  const lights: [string, boolean, string][] = [
    ['HPF', p.hpf.on, p.hpf.on ? `High-pass ${Math.round(p.hpf.hz)} Hz` : 'High-pass off'],
    ['G', p.gate.on, p.gate.on ? `Gate ${p.gate.threshold_db.toFixed(0)} dB` : 'Gate off'],
    ['EQ', p.eq.on, !p.eq.on ? 'EQ off' : eqActive ? 'EQ in' : 'EQ in, flat'],
    ['C', p.comp.on, p.comp.on ? `Compressor ${p.comp.threshold_db.toFixed(0)} dB, ${p.comp.ratio.toFixed(1)}:1` : 'Compressor off'],
  ]
  const order = p.order === 'comp_then_eq' ? ' · comp before EQ' : ''
  const delay = p.delay.on && p.delay.ms > 0 ? ` · delay ${p.delay.ms.toFixed(1)} ms` : ''
  return (
    <button
      type="button"
      className="proc-row"
      title={`${lights.map(([, , t]) => t).join(' · ')}${order}${delay}. Open the Selected Channel`}
      onClick={props.onSelect}
    >
      {lights.map(([label, on]) => (
        <span key={label} className={`proc-light${on ? ' on' : ''}`}>
          {label}
          {label === 'G' && <GateLight strip={stripKey(props.strip)} on={on} />}
        </span>
      ))}
    </button>
  )
}

/** The DCAs and mute groups a strip follows, as small chips. */
function Membership(props: { core: StripCore; dcas: Dca[]; groups: MuteGroup[] }) {
  const { core } = props
  const none = core.dcas.length === 0 && core.mute_groups.length === 0
  const names = [
    ...core.dcas.map((d) => props.dcas[d]?.name ?? `DCA ${d + 1}`),
    ...core.mute_groups.map((g) => props.groups[g]?.name ?? `Mute ${g + 1}`),
  ]
  return (
    <div className="strip-assign" title={none ? 'On no DCA or mute group' : names.join(', ')}>
      {[...core.dcas]
        .sort((a, b) => a - b)
        .map((d) => (
          <span
            key={`d${d}`}
            className={`assign-chip dca${props.dcas[d]?.mute ? ' muting' : ''}`}
            style={chipColorStyle(props.dcas[d]?.color ?? null)}
          >
            D{d + 1}
          </span>
        ))}
      {[...core.mute_groups]
        .sort((a, b) => a - b)
        .map((g) => (
          <span key={`m${g}`} className={`assign-chip mg${props.groups[g]?.active ? ' muting' : ''}`}>
            M{g + 1}
          </span>
        ))}
    </div>
  )
}

function StripName(props: {
  strip: StripRef
  name: string
  badge: ReactNode
  picked: boolean
  recallSafe: boolean
  onSelect: () => void
  onPick: (strip: StripRef, range: boolean) => void
  onHandle?: (e: ReactPointerEvent<HTMLElement>) => void
}) {
  return (
    <div className="strip-head">
      {props.onHandle ? (
        <span
          className="strip-badge handle"
          title="Drag to reorder · tap to open · Shift/Ctrl-tap to select"
          role="button"
          aria-label={`Move ${props.name}`}
          onPointerDown={props.onHandle}
        >
          {props.picked ? <Check size={10} strokeWidth={3} className="picked-mark" /> : <GripVertical size={10} className="grip" />}
          {props.badge}
        </span>
      ) : (
        <span className="strip-badge">
          {props.picked && <Check size={10} strokeWidth={3} className="picked-mark" />}
          {props.badge}
        </span>
      )}
      <button
        type="button"
        className="strip-name"
        aria-pressed={props.picked}
        title={`${props.name}: open the Selected Channel · Shift/Ctrl/⌘-click to select`}
        onClick={(e) => (picking(e) ? props.onPick(props.strip, e.shiftKey) : props.onSelect())}
      >
        {props.name}
      </button>
      {props.recallSafe && (
        <span className="safe-light on" title="Recall safe: scene recalls leave this strip alone">
          <Lock size={9} strokeWidth={2.5} />
        </span>
      )}
      <StripMenu strip={props.strip} name={props.name} recallSafe={props.recallSafe} />
    </div>
  )
}

function encodeInput(input: InputPatch): string {
  if (input.left === null) return ''
  return input.right === null ? `${input.left}` : `${input.left},${input.right}`
}

export function InputSelect(props: { channel: ChannelStrip; inputs: number }) {
  const current = encodeInput(props.channel.input)
  const options: [string, string][] = [['', 'No input']]
  for (let i = 0; i < props.inputs; i++) options.push([`${i}`, `In ${i + 1}`])
  for (let i = 0; i + 1 < props.inputs; i += 2) options.push([`${i},${i + 1}`, `In ${i + 1}-${i + 2}`])
  // Keep an input the device no longer has visible rather than lying.
  if (!options.some(([value]) => value === current)) {
    options.push([current, `In ${current.split(',').map((n) => Number(n) + 1).join('-')} (gone)`])
  }
  return (
    <Select
      value={current}
      icon={<Mic size={12} />}
      title="Input"
      className={current === '' ? 'unset' : ''}
      onChange={(value) => {
        const parts = value.split(',').filter(Boolean).map(Number)
        act({
          cmd: 'set_channel_input',
          channel: props.channel.id,
          input: { left: parts[0] ?? null, right: parts[1] ?? null },
        })
      }}
    >
      {options.map(([value, label]) => (
        <option key={value} value={value}>
          {label}
        </option>
      ))}
    </Select>
  )
}

function encodeOutput(output: StripOutput): string {
  return output.kind === 'bus' ? `bus:${output.id}` : output.kind
}

function OutputSelect(props: { strip: StripRef; output: StripOutput; buses: BusStrip[] }) {
  return (
    <Select
      value={encodeOutput(props.output)}
      icon={<ArrowRight size={12} />}
      title={
        props.strip.kind === 'bus'
          ? 'Output: the master, or only the outputs Patch → Outputs sends it to (a wedge, an IEM)'
          : 'Output'
      }
      onChange={(value) => {
        const output: StripOutput = value.startsWith('bus:')
          ? { kind: 'bus', id: Number(value.slice(4)) }
          : value === 'none'
            ? { kind: 'none' }
            : { kind: 'master' }
        act({ cmd: 'set_strip_output', strip: props.strip, output })
      }}
    >
      <option value="master">Master</option>
      {ROLES.map((role) => {
        const buses = props.buses.filter((b) => b.role === role.role)
        return buses.length === 0 ? null : (
          <optgroup key={role.role} label={role.bank}>
            {buses.map((bus) => (
              <option key={bus.id} value={`bus:${bus.id}`}>
                {bus.name}
              </option>
            ))}
          </optgroup>
        )
      })}
      <option value="none">{props.strip.kind === 'bus' ? 'Patch only' : 'Direct out only'}</option>
    </Select>
  )
}

function insertName(slot: InsertSlot, effects: Map<string, string>): string {
  return slot.plugin.type === 'builtin' ? (effects.get(slot.plugin.stem) ?? slot.plugin.stem) : slot.plugin.name
}

/** The built-in effects' names, by stem. */
function useEffectNames(): Map<string, string> {
  const hello = useStore((s) => s.hello)
  return useMemo(() => new Map(hello?.effects.map((e) => [e.stem, e.name]) ?? []), [hello])
}

/** A strip's inserts in one row: the first two by name (a tap opens the
 *  editor), the rest counted; the key at the end opens the whole rack —
 *  bypass, every slot, a new effect. With none, the row adds one. */
function InsertSummary(props: {
  inserts: InsertSlot[]
  states: Record<string, InsertState>
  onOpen: (insert: number) => void
  onAdd: () => void
}) {
  const names = useEffectNames()
  const { inserts } = props
  if (inserts.length === 0) {
    return (
      <button type="button" className="sum-row ins-add" title="Add an effect to this strip" onClick={props.onAdd}>
        <Plus size={11} /> Effect
      </button>
    )
  }
  const shown = inserts.slice(0, 2)
  const more = inserts.length - shown.length
  const all = inserts
    .map((s) => `${insertName(s, names)}${s.bypass ? ' (bypassed)' : ''}`)
    .join(', ')
  return (
    <div className="sum-row ins-sum" role="group" aria-label={`Inserts: ${all}`}>
      {shown.map((slot) => {
        const state = props.states[slot.id] ?? 'ready'
        const failed = typeof state === 'object'
        const name = insertName(slot, names)
        return (
          <button
            key={slot.id}
            type="button"
            className={`ins-chip${slot.bypass ? ' bypassed' : ''}${failed ? ' failed' : ''}`}
            title={failed ? `${name}: ${state.failed}` : `${name}${slot.bypass ? ' (bypassed)' : ''}: open its editor`}
            onClick={() => props.onOpen(slot.id)}
          >
            {state === 'loading' && <LoaderCircle size={9} className="spin ins-mark" />}
            {failed && <CircleAlert size={9} className="ins-mark" />}
            <span className="ins-name">{name}</span>
          </button>
        )
      })}
      <MenuButton
        popover
        popClass="rack-pop"
        className="sum-more"
        label={`Insert rack (${inserts.length}): ${all}. Bypass, open or add`}
        icon={more > 0 ? <span className="sum-more-count">+{more}</span> : <ChevronDown size={11} />}
      >
        {(close) => (
          <Inserts
            inserts={inserts}
            states={props.states}
            fit
            onOpen={(insert) => {
              close()
              props.onOpen(insert)
            }}
            onAdd={() => {
              close()
              props.onAdd()
            }}
          />
        )}
      </MenuButton>
    </div>
  )
}

/** A channel's sends in one row: the first by name and how many more; it
 *  opens the sends rack (levels, pre/post, removal, a new send). */
function SendSummary(props: { channel: ChannelStrip; buses: BusStrip[] }) {
  const { channel } = props
  const sends = channel.sends
  const first = sends.length > 0 ? props.buses.find((b) => b.id === sends[0].bus) : undefined
  const all = sends
    .map((s) => `${props.buses.find((b) => b.id === s.bus)?.name ?? '?'} ${formatDb(s.level_db)} ${s.pre_fader ? 'pre' : 'post'}`)
    .join(', ')
  return (
    <MenuButton
      popover
      popClass="rack-pop"
      className={`sum-row send-sum${sends.length === 0 ? ' empty' : ''}`}
      label={sends.length === 0 ? 'Sends: none. Open to add one' : `Sends (${sends.length}): ${all}. Open to set levels, pre/post, add`}
      icon={<ArrowUpRight size={11} className="send-sum-icon" />}
      text={
        <>
          <span className="send-sum-name">
            {sends.length === 0 ? (props.buses.length === 0 ? 'No buses' : 'Sends') : (first?.name ?? '?')}
          </span>
          {sends.length > 1 && <span className="sum-more-count">+{sends.length - 1}</span>}
          <ChevronDown size={11} className="sum-chevron" />
        </>
      }
    >
      {() => <Sends channel={channel} buses={props.buses} fit />}
    </MenuButton>
  )
}

export function Inserts(props: {
  inserts: InsertSlot[]
  states: Record<string, InsertState>
  onOpen: (insert: number) => void
  onAdd: () => void
  /** As tall as what it holds (a popover), not a fixed rack. */
  fit?: boolean
}) {
  const names = useEffectNames()
  const blanks = props.fit ? 0 : Math.max(0, RACK_ROWS - props.inserts.length - 1)
  return (
    <section className={`rack${props.fit ? ' fit' : ''}`}>
      <div className="rack-head">
        <span>Inserts</span>
        {props.inserts.length > 0 && <span className="rack-count">{props.inserts.length}</span>}
      </div>
      <div className="rack-list inserts">
        {props.inserts.map((slot) => {
          const state = props.states[slot.id] ?? 'ready'
          const failed = typeof state === 'object'
          return (
            <div
              key={slot.id}
              className={`slot${slot.bypass ? ' bypassed' : ''}${failed ? ' failed' : ''}`}
              title={failed ? state.failed : undefined}
            >
              <button
                type="button"
                className="slot-power"
                title={slot.bypass ? 'Bypassed: click to switch on' : 'Click to bypass'}
                onClick={() => act({ cmd: 'set_insert_bypass', insert: slot.id, bypass: !slot.bypass })}
              >
                <Power size={10} strokeWidth={2.75} />
              </button>
              <button type="button" className="slot-name" onClick={() => props.onOpen(slot.id)}>
                {insertName(slot, names)}
              </button>
              {state === 'loading' && <LoaderCircle size={11} className="spin" />}
              {failed && <CircleAlert size={11} className="slot-alert" />}
            </div>
          )
        })}
        <button type="button" className="slot slot-add" onClick={props.onAdd}>
          <Plus size={12} /> Effect
        </button>
        {Array.from({ length: blanks }, (_, i) => (
          <div key={i} className="slot slot-blank" />
        ))}
      </div>
    </section>
  )
}

/** A channel's sends: level, pre/post, removal, and a new one at silence
 *  (pre- or post-fader as the bus's role says). `wide` (the Selected
 *  Channel) adds each send's pan into a stereo bus. */
export function Sends(props: { channel: ChannelStrip; buses: BusStrip[]; wide?: boolean; fit?: boolean }) {
  const { channel } = props
  const phase3 = useStore((s) => s.phase3)
  const unused = props.buses.filter((bus) => !channel.sends.some((s) => s.bus === bus.id))
  return (
    <section className={`rack sends${props.wide ? ' wide' : ''}${props.fit ? ' fit' : ''}`}>
      <div className="rack-head">
        <span>Sends</span>
        {channel.sends.length > 0 && <span className="rack-count">{channel.sends.length}</span>}
      </div>
      <div className="rack-list send-list">
        {channel.sends.map((send) => {
          const bus = props.buses.find((b) => b.id === send.bus)
          const base = { cmd: 'set_send' as const, channel: channel.id, bus: send.bus }
          return (
            <div key={send.bus} className="send">
              <Knob
                value={send.level_db}
                min={MIN_FADER_DB}
                max={MAX_FADER_DB}
                defaultValue={0}
                size={24}
                hideValue
                label={`Send to ${bus?.name ?? 'bus'}`}
                format={formatDb}
                onChange={(level_db) =>
                  actLatest(`send:${channel.id}:${send.bus}`, { ...base, level_db, pre_fader: send.pre_fader })
                }
              />
              <div className="send-side">
                <span className="send-title">
                  {bus && props.wide && <RoleBadge role={bus.role} />}
                  <span className="send-name">{bus?.name ?? '?'}</span>
                  <button
                    type="button"
                    className="icon-button send-remove"
                    title="Remove this send"
                    onClick={() => act({ cmd: 'remove_send', channel: channel.id, bus: send.bus })}
                  >
                    <X size={10} />
                  </button>
                </span>
                <span className="send-meta">
                  <span className="value">{formatDb(send.level_db)}</span>
                  <button
                    type="button"
                    className={`pill${send.pre_fader ? ' on' : ''}`}
                    title="Pre-fader (monitor mix) or post-fader (effect send)"
                    onClick={() => act({ ...base, level_db: send.level_db, pre_fader: !send.pre_fader })}
                  >
                    {send.pre_fader ? 'PRE' : 'POST'}
                  </button>
                </span>
              </div>
              {props.wide && phase3 !== false && bus && (
                <SendPan
                  pan={send.pan}
                  follow={send.pan_follow}
                  pre={send.pre_fader}
                  channelPan={channel.pan}
                  stereo={bus.stereo}
                  to={bus.name}
                  onPan={(pan) =>
                    actLatest(`sendpan:${channel.id}:${send.bus}`, {
                      ...base,
                      level_db: send.level_db,
                      pre_fader: send.pre_fader,
                      pan,
                      pan_follow: false,
                    })
                  }
                  onFollow={() =>
                    act({
                      ...base,
                      level_db: send.level_db,
                      pre_fader: send.pre_fader,
                      pan: send.pan,
                      pan_follow: !send.pan_follow,
                    })
                  }
                />
              )}
            </div>
          )
        })}
        {channel.sends.length === 0 && (
          <div className="rack-empty">{props.buses.length === 0 ? 'Add a bus to send to' : 'No sends'}</div>
        )}
      </div>
      {unused.length > 0 && (
        <Select
          value=""
          icon={<Plus size={12} />}
          className="add-send"
          title="Add a send: it opens at silence, pre-fader to an aux, post-fader to a group or FX bus"
          onChange={(value) => {
            const bus = props.buses.find((b) => String(b.id) === value)
            if (!bus) return
            // Opens at silence, like a fader: it is turned up, not found loud.
            act({
              cmd: 'set_send',
              channel: channel.id,
              bus: bus.id,
              level_db: MIN_FADER_DB,
              pre_fader: roleInfo(bus.role).preFader,
            })
          }}
        >
          <option value="">Send</option>
          {ROLES.map((role) => {
            const buses = unused.filter((b) => b.role === role.role)
            return buses.length === 0 ? null : (
              <optgroup key={role.role} label={role.bank}>
                {buses.map((bus) => (
                  <option key={bus.id} value={bus.id}>
                    {bus.name}
                  </option>
                ))}
              </optgroup>
            )
          })}
        </Select>
      )}
    </section>
  )
}

/** A send's pan into a stereo bus: its own, or following the channel's. */
function SendPan(props: {
  pan: number
  follow: boolean
  pre: boolean
  channelPan: number
  stereo: boolean
  to: string
  onPan: (pan: number) => void
  onFollow: () => void
}) {
  if (!props.stereo) {
    return (
      <span className="send-pan mono" title={`${props.to} is mono: its sends have no pan`}>
        mono
      </span>
    )
  }
  const unpanned = props.follow && props.pre
  return (
    <span className={`send-pan${props.follow ? ' following' : ''}`}>
      <Knob
        value={unpanned ? 0 : props.follow ? props.channelPan : props.pan}
        min={-1}
        max={1}
        defaultValue={0}
        bipolar
        size={24}
        hideValue
        label={
          unpanned
            ? "Pre-fader and following: taken before the channel's pan, so unpanned. Turn to give the send its own"
            : props.follow
              ? "Follows the channel's pan: turn to give the send its own"
              : `Pan into ${props.to}`
        }
        format={formatPan}
        onChange={props.onPan}
      />
      <button
        type="button"
        className={`pill${props.follow ? ' on' : ''}`}
        aria-pressed={props.follow}
        title={props.follow ? "Follows the channel's pan. Click for its own pan" : "Its own pan. Click to follow the channel's"}
        onClick={props.onFollow}
      >
        {unpanned ? 'UNPANNED' : props.follow ? 'FOLLOW' : formatPan(props.pan)}
      </button>
    </span>
  )
}
