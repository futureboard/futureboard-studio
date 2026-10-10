// Scene memory (Phase 2), as a console's: the top bar's scene indicator
// (current scene, modified light, a cue that steps through the list and
// Recall), and the Scenes page — the list with numbers, names and notes,
// Recall, Store / Store as new, rename, note, delete, drag to reorder, Go
// previous / next, each scene's recall scope ("focus"), and the recall-safe
// overview.
//
// A scene's number is its place in the list. Recall is one undo step on the
// server; storing, renaming, scope and order are not undoable (as on a
// console's scene list).

import { useEffect, useRef, useState } from 'react'
import type { PointerEvent as ReactPointerEvent } from 'react'
import {
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  ChevronUp,
  Clapperboard,
  GripVertical,
  Lock,
  LockOpen,
  Pencil,
  Plus,
  Save,
  SkipBack,
  SkipForward,
  Trash2,
  TriangleAlert,
} from 'lucide-react'
import { ConfirmDialog, PromptDialog } from './Dialogs.tsx'
import { defaultScope } from './processing.ts'
import { roleInfo } from './routing.ts'
import type { Command, Id, RecallScope, SceneSummary, Session, StripRef } from './protocol.ts'
import { act, explain, notify, request, useStore } from './store.ts'
import './scenes.css'

// ── Helpers ─────────────────────────────────────────────────────────────

export const SCOPE_PARTS: { id: keyof RecallScope; label: string; detail: string }[] = [
  { id: 'faders', label: 'Faders', detail: 'Strip faders and DCA levels' },
  { id: 'mutes', label: 'Mutes', detail: 'Strip and DCA mutes, mute groups' },
  { id: 'pan', label: 'Pan', detail: 'Pan and balance' },
  { id: 'processing', label: 'Processing', detail: 'HPF, gate, EQ, comp, delay' },
  { id: 'inserts', label: 'Inserts', detail: 'Insert parameters and bypass' },
  { id: 'sends', label: 'Sends', detail: 'Send levels and pre/post' },
  { id: 'input', label: 'Input', detail: 'Trim, polarity, input patch' },
  { id: 'routing', label: 'Routing', detail: 'Strip outputs and output patch' },
  { id: 'assign', label: 'DCA & mute groups', detail: 'Which DCAs and mute groups strips follow' },
  { id: 'names', label: 'Names & colours', detail: 'Strip, DCA and mute-group names and colours' },
]

/** "03". */
export function sceneNumber(index: number): string {
  return String(index + 1).padStart(2, '0')
}

/** "03 Verse". */
export function sceneLabel(session: Session, id: Id): string {
  const index = session.scenes.findIndex((s) => s.id === id)
  return index < 0 ? 'scene' : `${sceneNumber(index)} ${session.scenes[index].name}`
}

/** "Everything", "No faders, mutes", "6 of 10 parts". */
export function scopeSummary(scope: RecallScope): string {
  const off = SCOPE_PARTS.filter((p) => !scope[p.id])
  if (off.length === 0) return 'Everything'
  if (off.length === SCOPE_PARTS.length) return 'Nothing'
  if (off.length <= 2) return `All but ${off.map((p) => p.label.toLowerCase()).join(', ')}`
  return `${SCOPE_PARTS.length - off.length} of ${SCOPE_PARTS.length} parts`
}

/** The scene the console is on: the server's 250 ms scene state when it has
 *  sent one, else the session's. */
export function useCurrentScene(session: Session): { current: Id | null; modified: boolean } {
  const sceneState = useStore((s) => s.sceneState)
  const current = sceneState ? sceneState.current : session.current_scene
  const exists = current !== null && session.scenes.some((s) => s.id === current)
  return { current: exists ? current : null, modified: exists && (sceneState?.modified ?? false) }
}

export async function recallScene(session: Session, id: Id) {
  const label = sceneLabel(session, id)
  const reply = await request({ cmd: 'scene_recall', id })
  if (!reply.ok) {
    notify(explain(reply.error), true)
    return
  }
  const note = typeof reply.note === 'string' && reply.note.trim() !== '' ? reply.note.trim() : null
  notify(note ? `Recalled ${label}. ${note}` : `Recalled ${label}`)
}

async function storeScene(session: Session, id: Id | null, name?: string) {
  // A new scene goes without an `id` at all (serde's default).
  const command: Command = id === null ? { cmd: 'scene_store' } : { cmd: 'scene_store', id }
  if (name !== undefined) command.name = name
  const reply = await request(command)
  if (!reply.ok) notify(explain(reply.error), true)
  else notify(id === null ? `Stored “${name ?? 'new scene'}”` : `Stored ${sceneLabel(session, id)}`)
}

// ── The top bar ─────────────────────────────────────────────────────────

/** The current scene ("03 Verse ● modified"), a cue the arrows step through
 *  the list, and Recall for the cued scene. The name opens the Scenes page. */
export function SceneBar(props: { session: Session; onOpen: () => void }) {
  const { session } = props
  const phase2 = useStore((s) => s.phase2)
  const { current, modified } = useCurrentScene(session)
  const scenes = session.scenes
  const [cue, setCue] = useState<Id | null>(null)
  // A recall from anywhere (here, another tablet) puts the cue back on it.
  useEffect(() => setCue(null), [current])
  const currentIndex = scenes.findIndex((s) => s.id === current)
  const cueIndex = cue !== null && scenes.some((s) => s.id === cue) ? scenes.findIndex((s) => s.id === cue) : currentIndex
  const shownIndex = cueIndex >= 0 ? cueIndex : scenes.length > 0 ? 0 : -1
  const shown = shownIndex >= 0 ? scenes[shownIndex] : null
  const cued = shown !== null && shown.id !== current

  if (phase2 === false) {
    return (
      <div className="scene-bar unavailable" title="This LiveStage server predates scenes, undo and the library. Update the server.">
        <Clapperboard size={14} />
        <span className="scene-bar-text">No scenes on this server</span>
      </div>
    )
  }

  const step = (delta: number) => {
    if (scenes.length === 0) return
    const from = shownIndex < 0 ? 0 : shownIndex
    const to = Math.max(0, Math.min(scenes.length - 1, from + delta))
    setCue(scenes[to].id)
  }

  return (
    <div className={`scene-bar${cued ? ' cued' : ''}`} role="group" aria-label="Scene">
      <button
        type="button"
        className="icon-button large"
        disabled={shownIndex <= 0}
        title="Cue the previous scene"
        aria-label="Cue the previous scene"
        onClick={() => step(-1)}
      >
        <ChevronLeft size={16} />
      </button>
      <button
        type="button"
        className="scene-current"
        title={
          shown
            ? `${cued ? `Cued: ${sceneNumber(shownIndex)} ${shown.name} — Recall to go there. ` : ''}${
                current !== null ? `On ${sceneLabel(session, current)}${modified ? ', changed since' : ''}. ` : 'No scene recalled yet. '
              }Open the scene list`
            : 'No scenes yet: open the scene list to store one'
        }
        onClick={props.onOpen}
      >
        {shown ? (
          <>
            {cued && <span className="scene-cue-tag">Cue</span>}
            <span className="scene-num value">{sceneNumber(shownIndex)}</span>
            <span className="scene-name">{shown.name}</span>
            {!cued && modified && (
              <span className="scene-modified">
                <span className="scene-modified-dot" aria-hidden />
                <span className="scene-modified-text">modified</span>
              </span>
            )}
          </>
        ) : (
          <span className="scene-name empty">No scenes</span>
        )}
      </button>
      <button
        type="button"
        className="icon-button large"
        disabled={shownIndex < 0 || shownIndex >= scenes.length - 1}
        title="Cue the next scene"
        aria-label="Cue the next scene"
        onClick={() => step(1)}
      >
        <ChevronRight size={16} />
      </button>
      <button
        type="button"
        className={`button small scene-recall${cued ? ' primary' : ''}`}
        disabled={!shown}
        title={shown ? `Recall ${sceneNumber(shownIndex)} ${shown.name} (${scopeSummary(shown.scope)})` : 'Store a scene first'}
        onClick={() => shown && void recallScene(session, shown.id)}
      >
        Recall
      </button>
    </div>
  )
}

// ── The page ────────────────────────────────────────────────────────────

/** How long a moved scene keeps its new place while the server confirms. */
const ORDER_HOLD_MS = 1500
const DRAG_START_PX = 5

export function ScenesPage(props: { session: Session }) {
  const { session } = props
  const phase2 = useStore((s) => s.phase2)
  const { current, modified } = useCurrentScene(session)
  const [open, setOpen] = useState<Id | null>(null)
  const [asking, setAsking] = useState<null | { kind: 'new' } | { kind: 'overwrite'; id: Id } | { kind: 'delete'; id: Id }>(null)
  // A scene just dropped keeps its place until the session agrees.
  const [order, setOrder] = useState<{ ids: Id[]; at: number } | null>(null)
  const [drag, setDrag] = useState<{ id: Id; dy: number; line: number | null } | null>(null)
  const list = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (!order) return
    if (session.scenes.map((s) => s.id).join() === order.ids.join()) {
      setOrder(null)
      return
    }
    const timer = window.setTimeout(() => setOrder(null), Math.max(0, order.at + ORDER_HOLD_MS - performance.now()))
    return () => window.clearTimeout(timer)
  }, [order, session.scenes])

  const scenes =
    order && order.ids.length === session.scenes.length
      ? order.ids.map((id) => session.scenes.find((s) => s.id === id)).filter((s): s is SceneSummary => !!s)
      : session.scenes
  const currentIndex = scenes.findIndex((s) => s.id === current)

  const move = (id: Id, index: number) => {
    const ids = scenes.map((s) => s.id)
    const rest = ids.filter((i) => i !== id)
    rest.splice(Math.max(0, Math.min(index, rest.length)), 0, id)
    if (rest.join() === ids.join()) return
    setOrder({ ids: rest, at: performance.now() })
    act({ cmd: 'scene_move', id, index })
  }

  const startDrag = (e: ReactPointerEvent<HTMLElement>, id: Id) => {
    const box = list.current
    if (e.button !== 0 || !box) return
    e.preventDefault()
    const handle = e.currentTarget
    handle.setPointerCapture(e.pointerId)
    const startY = e.clientY
    let active = false
    let slot: number | null = null
    const rows = () => [...box.querySelectorAll<HTMLElement>('[data-scene]')].filter((el) => Number(el.dataset.scene) !== id)
    const onMove = (ev: PointerEvent) => {
      if (!active && Math.abs(ev.clientY - startY) < DRAG_START_PX) return
      active = true
      const others = rows()
      slot = others.filter((el) => {
        const r = el.getBoundingClientRect()
        return ev.clientY > r.top + r.height / 2
      }).length
      const top = box.getBoundingClientRect().top
      const line =
        others.length === 0
          ? null
          : slot < others.length
            ? others[slot].getBoundingClientRect().top - top - 2
            : others[others.length - 1].getBoundingClientRect().bottom - top + 1
      setDrag({ id, dy: ev.clientY - startY, line })
    }
    const finish = (commit: boolean) => {
      handle.removeEventListener('pointermove', onMove)
      handle.removeEventListener('pointerup', onUp)
      handle.removeEventListener('pointercancel', onCancel)
      setDrag(null)
      if (commit && active && slot !== null) move(id, slot)
    }
    const onUp = () => finish(true)
    const onCancel = () => finish(false)
    handle.addEventListener('pointermove', onMove)
    handle.addEventListener('pointerup', onUp)
    handle.addEventListener('pointercancel', onCancel)
  }

  if (phase2 === false) {
    return (
      <div className="page scenes-page">
        <section className="card">
          <header className="card-head">
            <Clapperboard size={16} />
            <div>
              <h2>Scenes</h2>
              <p>This LiveStage server predates scenes, recall safe, undo and the library. Update the server to use them.</p>
            </div>
          </header>
        </section>
      </div>
    )
  }

  const go = (delta: number) => {
    if (scenes.length === 0) return
    const from = currentIndex < 0 ? (delta > 0 ? -1 : scenes.length) : currentIndex
    const to = from + delta
    if (to < 0 || to >= scenes.length) return
    void recallScene(session, scenes[to].id)
  }

  return (
    <div className="page scenes-page">
      <section className="card">
        <header className="card-head scenes-head">
          <Clapperboard size={16} />
          <div className="scenes-head-text">
            <h2>Scenes</h2>
            <p>
              {scenes.length === 0
                ? 'No scenes yet. Store the mix as it is now to make the first.'
                : current !== null
                  ? `On ${sceneLabel(session, current)}${modified ? ' — changed since it was stored or recalled' : ''}`
                  : `${scenes.length} scene${scenes.length === 1 ? '' : 's'}; none recalled yet`}
            </p>
          </div>
        </header>
        <div className="scenes-actions">
          <div className="scenes-go">
            <button
              type="button"
              className="button"
              disabled={scenes.length === 0 || currentIndex === 0}
              title="Recall the scene before the current one"
              onClick={() => go(-1)}
            >
              <SkipBack size={14} /> Go previous
            </button>
            <button
              type="button"
              className="button"
              disabled={scenes.length === 0 || currentIndex === scenes.length - 1}
              title="Recall the scene after the current one"
              onClick={() => go(1)}
            >
              Go next <SkipForward size={14} />
            </button>
          </div>
          <span className="spacer" />
          <button
            type="button"
            className="button"
            disabled={current === null}
            title={current !== null ? `Store the mix into ${sceneLabel(session, current)}, replacing what it holds` : 'Recall or store a scene first'}
            onClick={() => current !== null && setAsking({ kind: 'overwrite', id: current })}
          >
            <Save size={14} /> Store
          </button>
          <button type="button" className="button primary" title="Store the mix as it is now as a new scene, after the current one" onClick={() => setAsking({ kind: 'new' })}>
            <Plus size={14} /> Store as new
          </button>
        </div>

        {scenes.length > 0 && (
          <div className={`scene-list${drag ? ' dragging' : ''}`} ref={list}>
            {scenes.map((scene, index) => (
              <SceneRow
                key={scene.id}
                session={session}
                scene={scene}
                index={index}
                count={scenes.length}
                current={scene.id === current}
                modified={scene.id === current && modified}
                open={open === scene.id}
                dragY={drag?.id === scene.id ? drag.dy : null}
                onToggle={() => setOpen(open === scene.id ? null : scene.id)}
                onHandle={(e) => startDrag(e, scene.id)}
                onMove={(to) => move(scene.id, to)}
                onStore={() => setAsking({ kind: 'overwrite', id: scene.id })}
                onDelete={() => setAsking({ kind: 'delete', id: scene.id })}
              />
            ))}
            {drag && drag.line !== null && <div className="scene-drop-line" style={{ top: drag.line }} />}
          </div>
        )}
      </section>

      <RecallSafeCard session={session} />

      {asking?.kind === 'new' && (
        <PromptDialog
          icon={<Plus size={16} />}
          title="Store as a new scene"
          label="Name"
          initial={`Scene ${session.scenes.length + 1}`}
          confirmLabel="Store"
          hint={
            current !== null
              ? `The mix as it is now, placed after ${sceneLabel(session, current)}.`
              : 'The mix as it is now, at the end of the list.'
          }
          onConfirm={(name) => void storeScene(session, null, name)}
          onCancel={() => setAsking(null)}
        />
      )}
      {asking?.kind === 'overwrite' && (
        <ConfirmDialog
          icon={<Save size={16} />}
          title={`Store into ${sceneLabel(session, asking.id)}?`}
          confirmLabel="Store"
          onConfirm={() => void storeScene(session, asking.id)}
          onCancel={() => setAsking(null)}
        >
          What the scene holds is replaced by the mix as it is now. Its name, note and recall scope stay. This cannot be undone.
        </ConfirmDialog>
      )}
      {asking?.kind === 'delete' && (
        <ConfirmDialog
          icon={<Trash2 size={16} />}
          title={`Delete ${sceneLabel(session, asking.id)}?`}
          confirmLabel="Delete"
          danger
          onConfirm={() => {
            if (open === asking.id) setOpen(null)
            act({ cmd: 'scene_delete', id: asking.id })
          }}
          onCancel={() => setAsking(null)}
        >
          The scene goes from the list; the scenes after it move up a number. The mix is not touched. This cannot be undone.
        </ConfirmDialog>
      )}
    </div>
  )
}

function SceneRow(props: {
  session: Session
  scene: SceneSummary
  index: number
  count: number
  current: boolean
  modified: boolean
  open: boolean
  dragY: number | null
  onToggle: () => void
  onHandle: (e: ReactPointerEvent<HTMLElement>) => void
  onMove: (index: number) => void
  onStore: () => void
  onDelete: () => void
}) {
  const { session, scene, index } = props
  const [renaming, setRenaming] = useState(false)
  const dragged = props.dragY !== null
  const partial = SCOPE_PARTS.some((p) => !scene.scope[p.id])
  return (
    <div
      className={`scene-row${props.current ? ' current' : ''}${props.open ? ' open' : ''}${dragged ? ' dragged' : ''}`}
      data-scene={scene.id}
      style={dragged ? { transform: `translateY(${props.dragY}px)` } : undefined}
    >
      <div className="scene-line">
        <span
          className="scene-grip"
          role="button"
          aria-label={`Move ${scene.name}`}
          title="Drag to reorder"
          onPointerDown={props.onHandle}
        >
          <GripVertical size={14} />
        </span>
        <span className="scene-row-num value">{sceneNumber(index)}</span>
        <button type="button" className="scene-row-text" onClick={props.onToggle} aria-expanded={props.open}>
          <span className="scene-row-name">
            {scene.name}
            {props.current && <span className="scene-tag">Current</span>}
            {props.modified && (
              <span className="scene-modified" title="Changed since it was stored or recalled">
                <span className="scene-modified-dot" aria-hidden />
                <span className="scene-modified-text">modified</span>
              </span>
            )}
          </span>
          {scene.note && <span className="scene-row-note">{scene.note}</span>}
        </button>
        <span className={`scene-scope-chip${partial ? ' partial' : ''}`} title={`A recall touches: ${scopeSummary(scene.scope)}`}>
          {partial ? 'Focus' : 'All'}
          <span className="scene-scope-chip-text"> · {scopeSummary(scene.scope)}</span>
        </span>
        <button
          type="button"
          className="button small"
          title={`Recall ${sceneNumber(index)} ${scene.name}: ${scopeSummary(scene.scope)}`}
          onClick={() => void recallScene(session, scene.id)}
        >
          Recall
        </button>
        <button
          type="button"
          className="icon-button large"
          title={props.open ? 'Close' : 'Name, note, scope and more'}
          aria-label={props.open ? 'Close' : `Edit ${scene.name}`}
          aria-expanded={props.open}
          onClick={props.onToggle}
        >
          {props.open ? <ChevronUp size={16} /> : <ChevronDown size={16} />}
        </button>
      </div>
      {props.open && (
        <div className="scene-detail">
          <div className="scene-detail-row">
            <button type="button" className="button small" onClick={() => setRenaming(true)}>
              <Pencil size={13} /> Rename
            </button>
            <button type="button" className="button small" title="Replace what it holds with the mix as it is now" onClick={props.onStore}>
              <Save size={13} /> Store here
            </button>
            <button type="button" className="button small" disabled={index === 0} title="One place earlier" onClick={() => props.onMove(index - 1)}>
              <ChevronUp size={13} /> Up
            </button>
            <button
              type="button"
              className="button small"
              disabled={index === props.count - 1}
              title="One place later"
              onClick={() => props.onMove(index + 1)}
            >
              <ChevronDown size={13} /> Down
            </button>
            <span className="spacer" />
            <button type="button" className="button small danger" onClick={props.onDelete}>
              <Trash2 size={13} /> Delete
            </button>
          </div>
          <NoteField scene={scene} />
          <ScopeEditor scene={scene} />
        </div>
      )}
      {renaming && (
        <PromptDialog
          icon={<Pencil size={16} />}
          title={`Rename ${sceneNumber(index)}`}
          label="Name"
          initial={scene.name}
          confirmLabel="Rename"
          onConfirm={(name) => name !== scene.name && act({ cmd: 'scene_rename', id: scene.id, name })}
          onCancel={() => setRenaming(false)}
        />
      )}
    </div>
  )
}

/** The scene's note: committed on leaving the field. */
function NoteField(props: { scene: SceneSummary }) {
  const { scene } = props
  return (
    <label className="dialog-field scene-note-field">
      <span>Note</span>
      <textarea
        key={scene.note}
        className="text-input text-area"
        rows={2}
        defaultValue={scene.note}
        placeholder="Cue notes: who is on stage, what changes…"
        onBlur={(e) => {
          const note = e.currentTarget.value
          if (note !== scene.note) act({ cmd: 'scene_note', id: scene.id, note })
        }}
        onKeyDown={(e) => {
          if (e.key === 'Escape') {
            e.currentTarget.value = scene.note
            e.currentTarget.blur()
          }
          if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) e.currentTarget.blur()
        }}
      />
    </label>
  )
}

/** The ten parts a recall of this scene touches. */
function ScopeEditor(props: { scene: SceneSummary }) {
  const { scene } = props
  const set = (scope: RecallScope) => act({ cmd: 'scene_scope', id: scene.id, scope })
  const all = SCOPE_PARTS.every((p) => scene.scope[p.id])
  const none = SCOPE_PARTS.every((p) => !scene.scope[p.id])
  const nothing = Object.fromEntries(SCOPE_PARTS.map((p) => [p.id, false])) as unknown as RecallScope
  return (
    <div className="scope">
      <div className="scope-head">
        <span className="scope-title">Recall touches</span>
        <span className="scope-summary">{scopeSummary(scene.scope)}</span>
        <span className="spacer" />
        <button type="button" className="button small" disabled={all} onClick={() => set(defaultScope())}>
          All
        </button>
        <button type="button" className="button small" disabled={none} onClick={() => set(nothing)}>
          None
        </button>
      </div>
      <div className="scope-grid">
        {SCOPE_PARTS.map((part) => {
          const on = scene.scope[part.id]
          return (
            <button
              key={part.id}
              type="button"
              role="switch"
              aria-checked={on}
              className={`scope-part${on ? ' on' : ''}`}
              title={`${part.detail}: ${on ? 'recalled' : 'left as it is on recall'}`}
              onClick={() => set({ ...scene.scope, [part.id]: !on })}
            >
              <span className="scope-light" aria-hidden />
              <span className="scope-text">
                <span className="scope-label">{part.label}</span>
                <span className="scope-detail">{part.detail}</span>
              </span>
              <span className="scope-state">{on ? 'Recalled' : 'Kept'}</span>
            </button>
          )
        })}
      </div>
      <span className="dialog-hint">Solo is never recalled. Recall-safe strips are left alone whatever the scope.</span>
    </div>
  )
}

// ── Recall safe ─────────────────────────────────────────────────────────

interface Safed {
  strip: StripRef
  name: string
  kind: string
}

function safedStrips(session: Session): Safed[] {
  return [
    ...session.channels
      .map((c, i) => ({ c, i }))
      .filter(({ c }) => c.recall_safe)
      .map(({ c, i }): Safed => ({ strip: { kind: 'channel', id: c.id }, name: c.name, kind: `Channel ${i + 1}` })),
    ...session.buses
      .filter((b) => b.recall_safe)
      .map((b): Safed => ({ strip: { kind: 'bus', id: b.id }, name: b.name, kind: roleInfo(b.role).label })),
    ...session.matrices
      .filter((m) => m.recall_safe)
      .map((m): Safed => ({ strip: { kind: 'matrix', id: m.id }, name: m.name, kind: 'Matrix' })),
    ...(session.master.recall_safe ? [{ strip: { kind: 'master' } as StripRef, name: 'Master', kind: 'Master' }] : []),
  ]
}

/** Every recall-safe strip, with a quick way to un-safe it. */
export function RecallSafeCard(props: { session: Session }) {
  const safed = safedStrips(props.session)
  return (
    <section className="card">
      <header className="card-head">
        <Lock size={16} />
        <div>
          <h2>Recall safe</h2>
          <p>A recall-safe strip is left entirely alone by every scene recall. Set it in the strip's ⋮ menu or its Selected Channel.</p>
        </div>
      </header>
      {safed.length === 0 ? (
        <div className="card-empty">No strip is recall safe: every recall reaches every strip (within its scope).</div>
      ) : (
        <>
          <div className="safe-list">
            {safed.map((s) => (
              <div key={s.kind + s.name + JSON.stringify(s.strip)} className="safe-row">
                <span className="safe-light on" aria-hidden>
                  <Lock size={11} />
                </span>
                <span className="safe-name">{s.name}</span>
                <span className="safe-kind">{s.kind}</span>
                <button
                  type="button"
                  className="button small"
                  title={`Let scene recalls reach ${s.name} again`}
                  onClick={() => act({ cmd: 'set_recall_safe', strip: s.strip, safe: false })}
                >
                  <LockOpen size={13} /> Un-safe
                </button>
              </div>
            ))}
          </div>
          {safed.length > 1 && (
            <div className="card-actions">
              <span className="muted">
                <TriangleAlert size={13} /> {safed.length} strips ignore recalls
              </span>
              <span className="spacer" />
              <button
                type="button"
                className="button small"
                onClick={() => {
                  for (const s of safed) act({ cmd: 'set_recall_safe', strip: s.strip, safe: false })
                }}
              >
                Un-safe all
              </button>
            </div>
          )}
        </>
      )}
    </section>
  )
}
