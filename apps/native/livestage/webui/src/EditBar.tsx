// The top bar's edit and save controls (Phase 2): Undo and Redo, labelled
// from the server's `history` message and on Ctrl/⌘+Z, Shift+Ctrl/⌘+Z and
// Ctrl+Y; and the saved indicator beside Save ("Saved 12 s ago" /
// "Unsaved changes") from the server's `saved` messages and the sessions it
// pushes since.

import { useEffect, useState } from 'react'
import { Redo2, Save, Undo2 } from 'lucide-react'
import { explain, getState, markSaved, notify, request, useStore } from './store.ts'

const NO_UNDO = 'This LiveStage server predates undo. Update the server.'

export async function undo() {
  const { history, phase2 } = getState()
  if (phase2 === false) {
    notify(NO_UNDO, true)
    return
  }
  if (history && !history.undo) return
  const label = history?.undo
  const reply = await request({ cmd: 'undo' })
  // A reply's note says what happened ("Nothing to undo") better than the label.
  if (reply.ok) notify(typeof reply.note === 'string' && reply.note ? reply.note : label ? `Undid ${label}` : 'Undone')
  else notify(explain(reply.error), true)
}

export async function redo() {
  const { history, phase2 } = getState()
  if (phase2 === false) {
    notify(NO_UNDO, true)
    return
  }
  if (history && !history.redo) return
  const label = history?.redo
  const reply = await request({ cmd: 'redo' })
  // A reply's note says what happened ("Nothing to undo") better than the label.
  if (reply.ok) notify(typeof reply.note === 'string' && reply.note ? reply.note : label ? `Redid ${label}` : 'Redone')
  else notify(explain(reply.error), true)
}

/** Where a key press is text editing, so Ctrl+Z belongs to the field. */
function typing(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null
  if (!el) return false
  if (el.isContentEditable) return true
  const tag = el.tagName
  if (tag === 'TEXTAREA' || tag === 'SELECT') return true
  if (tag !== 'INPUT') return false
  const type = (el as HTMLInputElement).type
  return !['button', 'checkbox', 'radio', 'range', 'submit', 'reset'].includes(type)
}

/** Ctrl/⌘+Z undo, Shift+Ctrl/⌘+Z and Ctrl+Y redo, anywhere but a field. */
export function useUndoKeys() {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey) || e.altKey || typing(e.target)) return
      const key = e.key.toLowerCase()
      if (key === 'z') {
        e.preventDefault()
        void (e.shiftKey ? redo() : undo())
      } else if (key === 'y' && !e.shiftKey) {
        e.preventDefault()
        void redo()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])
}

const MAC = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform)
const MOD = MAC ? '⌘' : 'Ctrl+'

export function HistoryButtons() {
  const history = useStore((s) => s.history)
  const phase2 = useStore((s) => s.phase2)
  const unavailable = phase2 === false
  const undoTitle = unavailable
    ? NO_UNDO
    : history?.undo
      ? `Undo ${history.undo} (${MOD}Z)${history.undo_depth > 1 ? ` · ${history.undo_depth} steps` : ''}`
      : 'Nothing to undo'
  const redoTitle = unavailable
    ? NO_UNDO
    : history?.redo
      ? `Redo ${history.redo} (${MAC ? '⇧⌘Z' : 'Ctrl+Y'})${history.redo_depth > 1 ? ` · ${history.redo_depth} steps` : ''}`
      : 'Nothing to redo'
  return (
    <div className="history" role="group" aria-label="Undo and redo">
      <button
        type="button"
        className="button icon-only"
        disabled={unavailable || !history?.undo}
        title={undoTitle}
        aria-label={undoTitle}
        onClick={() => void undo()}
      >
        <Undo2 size={15} />
      </button>
      <button
        type="button"
        className="button icon-only"
        disabled={unavailable || !history?.redo}
        title={redoTitle}
        aria-label={redoTitle}
        onClick={() => void redo()}
      >
        <Redo2 size={15} />
      </button>
    </div>
  )
}

function ago(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000))
  if (s < 5) return 'just now'
  if (s < 60) return `${s} s ago`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m} min ago`
  const h = Math.floor(m / 60)
  return `${h} h ago`
}

/** Save, and beside it what the session file holds: "Saved 12 s ago",
 *  "Unsaved changes", or nothing when this page has seen neither. */
export function SaveControls() {
  const hello = useStore((s) => s.hello)
  const saved = useStore((s) => s.saved)
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (saved.at === null) return
    const timer = window.setInterval(() => setNow(Date.now()), 1000)
    return () => window.clearInterval(timer)
  }, [saved.at])
  const path = hello?.session_path ?? null

  const save = async () => {
    const reply = await request({ cmd: 'save' })
    if (reply.ok) {
      markSaved(typeof reply.path === 'string' ? reply.path : path, Date.now(), false)
      notify(`Saved ${String(reply.path)}`)
    } else {
      notify(explain(reply.error), true)
    }
  }

  let text: string | null = null
  let state: 'saved' | 'dirty' | 'none' = 'none'
  if (!path) {
    text = 'No session file'
  } else if (saved.dirty) {
    text = 'Unsaved changes'
    state = 'dirty'
  } else if (saved.at !== null) {
    text = `Saved ${ago(now - saved.at)}`
    state = 'saved'
  }
  const when = saved.at !== null ? new Date(saved.at).toLocaleTimeString() : null
  const title = !path
    ? 'The server was started without --session: nothing is saved'
    : `${state === 'dirty' ? 'Changed since the last save. ' : ''}${
        when ? `Last ${saved.auto ? 'autosaved' : 'saved'} at ${when}. ` : ''
      }Save to ${path}`

  return (
    <div className={`save-controls ${state}`}>
      {text && (
        <span className="save-state" title={title}>
          {state !== 'none' && <span className="save-dot" aria-hidden />}
          <span className="save-text">{text}</span>
        </span>
      )}
      <button type="button" className="button icon-only save-button" disabled={!path} title={title} aria-label={title} onClick={() => void save()}>
        <Save size={15} />
        {state === 'dirty' && <span className="save-badge" aria-hidden />}
      </button>
    </div>
  )
}
