// Copy, paste and selection (Phase 2): the strip menu, the selection
// cluster over the mixer, the section picker and the Copy dialog, and the
// host that shows whichever workflow dialog is open.
//
// A copy is built here from the session the page has (workstate.ts); a paste
// is one `paste_strip` command, one undo step on the server.

import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { createPortal } from 'react-dom'
import {
  BookOpen,
  Check,
  ClipboardCopy,
  ClipboardPaste,
  Copy,
  EllipsisVertical,
  Lock,
  LockOpen,
  SquareCheckBig,
  Trash2,
  X,
} from 'lucide-react'
import { ConfirmDialog } from './Dialogs.tsx'
import { Modal } from './Inserts.tsx'
import { LibraryDialog } from './Library.tsx'
import { SectionPicker } from './Sections.tsx'
import type { Section, Session, StripRef } from './protocol.ts'
import { stripKey } from './protocol.ts'
import { act, explain, notify, request, useStore } from './store.ts'
import type { Clip } from './workstate.ts'
import './workflow.css'
import {
  DEFAULT_SECTIONS,
  clearSelection,
  closeDialog,
  copyStrip,
  openDialog,
  sectionsFor,
  sectionsLabel,
  selectedStrips,
  setSelectMode,
  stripExists,
  stripLabel,
  stripsLabel,
  targetsFor,
  useWork,
} from './workstate.ts'

// ── Actions ─────────────────────────────────────────────────────────────

export async function pasteClip(session: Session, targets: StripRef[], clip: Clip) {
  if (targets.length === 0) return
  const reply = await request({
    cmd: 'paste_strip',
    targets,
    settings: clip.settings,
    sections: clip.sections,
  })
  if (reply.ok) {
    notify(`Pasted ${sectionsLabel(clip.sections)} from ${clip.source} to ${stripsLabel(session, targets)}`)
  } else {
    notify(explain(reply.error), true)
  }
}

export function copyDefault(session: Session, strip: StripRef) {
  const clip = copyStrip(session, strip, DEFAULT_SECTIONS)
  if (clip) notify(`Copied ${sectionsLabel(clip.sections)} from ${clip.source}`)
}

/** "Paste EQ · Comp from Kick": what a paste button will do. */
export function pasteLabel(clip: Clip | null): string {
  return clip ? `${sectionsLabel(clip.sections)} from ${clip.source}` : 'Nothing copied yet'
}

// ── An anchored menu ────────────────────────────────────────────────────

/** A button that opens a menu under it: anchored to its measured bounds,
 *  clamped to the window, closed by a pick, Escape, a press outside, or the
 *  page scrolling under it. */
export function MenuButton(props: {
  label: string
  icon: ReactNode
  className?: string
  text?: ReactNode
  /** A popover of controls (a rack, a list of latches) rather than a menu
   *  of commands: a dialog role, and `popClass` on its plate. */
  popover?: boolean
  popClass?: string
  children: (close: () => void) => ReactNode
}) {
  const [open, setOpen] = useState(false)
  const button = useRef<HTMLButtonElement>(null)
  const menu = useRef<HTMLDivElement>(null)
  const [place, setPlace] = useState<{ left: number; top: number } | null>(null)
  const close = () => setOpen(false)

  useLayoutEffect(() => {
    if (!open || !button.current || !menu.current) return
    const r = button.current.getBoundingClientRect()
    const m = menu.current.getBoundingClientRect()
    const margin = 8
    let left = r.right - m.width
    if (left < margin) left = Math.min(r.left, window.innerWidth - m.width - margin)
    left = Math.max(margin, Math.min(left, window.innerWidth - m.width - margin))
    let top = r.bottom + 4
    if (top + m.height > window.innerHeight - margin) top = Math.max(margin, r.top - m.height - 4)
    setPlace({ left, top })
  }, [open])

  useEffect(() => {
    if (!open) {
      setPlace(null)
      return
    }
    const onDown = (e: PointerEvent) => {
      const target = e.target as Node
      if (!menu.current?.contains(target) && !button.current?.contains(target)) setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation()
        setOpen(false)
        button.current?.focus()
      }
    }
    const onScroll = (e: Event) => {
      if (!menu.current?.contains(e.target as Node)) setOpen(false)
    }
    window.addEventListener('pointerdown', onDown, true)
    window.addEventListener('keydown', onKey, true)
    window.addEventListener('scroll', onScroll, true)
    window.addEventListener('resize', close)
    return () => {
      window.removeEventListener('pointerdown', onDown, true)
      window.removeEventListener('keydown', onKey, true)
      window.removeEventListener('scroll', onScroll, true)
      window.removeEventListener('resize', close)
    }
  }, [open])

  return (
    <>
      <button
        ref={button}
        type="button"
        className={props.className ?? 'icon-button'}
        title={props.label}
        aria-label={props.label}
        aria-haspopup={props.popover ? 'dialog' : 'menu'}
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        {props.icon}
        {props.text}
      </button>
      {open &&
        createPortal(
          <div
            ref={menu}
            className={`menu${props.popover ? ' popover' : ''}${props.popClass ? ` ${props.popClass}` : ''}`}
            role={props.popover ? 'dialog' : 'menu'}
            aria-label={props.popover ? props.label : undefined}
            style={place ? { left: place.left, top: place.top } : { left: 0, top: 0, visibility: 'hidden' }}
          >
            {props.children(close)}
          </div>,
          document.body,
        )}
    </>
  )
}

export function MenuItem(props: {
  icon?: ReactNode
  label: ReactNode
  detail?: ReactNode
  onClick: () => void
  disabled?: boolean
  danger?: boolean
  checked?: boolean
  title?: string
}) {
  return (
    <button
      type="button"
      role={props.checked === undefined ? 'menuitem' : 'menuitemcheckbox'}
      aria-checked={props.checked}
      className={`menu-item${props.danger ? ' danger' : ''}`}
      disabled={props.disabled}
      title={props.title}
      onClick={props.onClick}
    >
      <span className="menu-icon">{props.icon}</span>
      <span className="menu-text">
        <span className="menu-label">{props.label}</span>
        {props.detail && <span className="menu-detail">{props.detail}</span>}
      </span>
      {props.checked !== undefined && <span className="menu-check">{props.checked && <Check size={13} />}</span>}
    </button>
  )
}

export function MenuSeparator() {
  return <div className="menu-separator" role="separator" />
}

// ── A strip's menu ──────────────────────────────────────────────────────

/** Copy, paste, library, recall safe and removal for one strip: the strip's
 *  ⋮ in the mixer. Acts on the selection when the strip is part of it. */
export function StripMenu(props: { strip: StripRef; name: string; recallSafe: boolean }) {
  const { strip } = props
  const [removing, setRemoving] = useState(false)
  return (
    <>
      <MenuButton label={`${props.name}: copy, paste, library…`} icon={<EllipsisVertical size={13} />} className="icon-button strip-menu">
        {(close) => <StripMenuItems strip={strip} name={props.name} recallSafe={props.recallSafe} close={close} onRemove={() => setRemoving(true)} />}
      </MenuButton>
      {removing && (
        <ConfirmDialog
          icon={<Trash2 size={16} />}
          title={`Remove ${props.name}?`}
          confirmLabel="Remove"
          danger
          onCancel={() => setRemoving(false)}
          onConfirm={() => {
            if (strip.kind === 'channel') act({ cmd: 'remove_channel', channel: strip.id })
            if (strip.kind === 'bus') act({ cmd: 'remove_bus', bus: strip.id })
            if (strip.kind === 'matrix') act({ cmd: 'remove_matrix', matrix: strip.id })
          }}
        >
          {strip.kind === 'matrix'
            ? 'The matrix, its inserts and its output patch go. Undo brings it back.'
            : 'The strip, its inserts and its sends go. Undo brings it back.'}
        </ConfirmDialog>
      )}
    </>
  )
}

function StripMenuItems(props: {
  strip: StripRef
  name: string
  recallSafe: boolean
  close: () => void
  onRemove: () => void
}) {
  const { strip, close } = props
  const session = useStore((s) => s.session)
  const phase2 = useStore((s) => s.phase2)
  const clip = useWork((w) => w.clip)
  if (!session) return null
  const targets = targetsFor(session, strip)
  const many = targets.length > 1
  const unavailable = phase2 === false ? 'This server predates paste, scenes and the library' : undefined
  return (
    <>
      <div className="menu-head">{many ? `${props.name} · ${targets.length} selected` : props.name}</div>
      <MenuItem
        icon={<Copy size={14} />}
        label="Copy processing"
        detail="HPF, gate, EQ, comp, delay"
        onClick={() => {
          close()
          copyDefault(session, strip)
        }}
      />
      <MenuItem
        icon={<ClipboardCopy size={14} />}
        label="Copy…"
        detail="Choose the sections"
        onClick={() => {
          close()
          openDialog({ kind: 'copy', strip })
        }}
      />
      <MenuItem
        icon={<ClipboardPaste size={14} />}
        label={many ? `Paste to ${targets.length} strips` : 'Paste'}
        detail={pasteLabel(clip)}
        disabled={!clip || phase2 === false}
        title={unavailable}
        onClick={() => {
          close()
          if (clip) void pasteClip(session, targets, clip)
        }}
      />
      <MenuSeparator />
      <MenuItem
        icon={<BookOpen size={14} />}
        label="Library…"
        detail={many ? `Apply to ${targets.length} strips, or save this one` : 'Apply, or save this strip'}
        onClick={() => {
          close()
          openDialog({ kind: 'library', source: strip, targets })
        }}
      />
      <MenuItem
        icon={props.recallSafe ? <Lock size={14} /> : <LockOpen size={14} />}
        label="Recall safe"
        detail="Scene recalls leave it alone"
        checked={props.recallSafe}
        disabled={phase2 === false}
        title={unavailable}
        onClick={() => {
          close()
          act({ cmd: 'set_recall_safe', strip, safe: !props.recallSafe })
        }}
      />
      {strip.kind !== 'master' && (
        <>
          <MenuSeparator />
          <MenuItem
            icon={<Trash2 size={14} />}
            label={`Remove ${props.name}`}
            danger
            onClick={() => {
              close()
              props.onRemove()
            }}
          />
        </>
      )}
    </>
  )
}

// ── The selection, over the mixer ───────────────────────────────────────

/** Select mode, for touch: while on, a tap on a strip's name selects it
 *  instead of opening it. Shift/Ctrl/⌘-click selects at any time. */
export function SelectModeButton() {
  const selectMode = useWork((w) => w.selectMode)
  return (
    <button
      type="button"
      className={`select-mode${selectMode ? ' on' : ''}`}
      aria-pressed={selectMode}
      title={
        selectMode
          ? 'Select mode: a tap on a strip name selects it. Tap here to go back to opening strips'
          : 'Select strips by tapping their names (or Shift/Ctrl/⌘-click any time)'
      }
      onClick={() => setSelectMode(!selectMode)}
    >
      <SquareCheckBig size={14} />
      <span>Select</span>
    </button>
  )
}

/** The selection's count, Paste and Library to it, and clearing it; shown
 *  while strips are selected. */
export function SelectionCluster(props: { session: Session }) {
  const { session } = props
  const selection = useWork((w) => w.selection)
  const clip = useWork((w) => w.clip)
  const phase2 = useStore((s) => s.phase2)
  const strips = selectedStrips(session, selection)
  const count = strips.length
  if (count === 0) return null
  return (
    <div className="console-cluster selection" role="group" aria-label="Selection">
      <span className="selection-count value" title={stripsLabel(session, strips)}>
        {count} selected
      </span>
      <button
        type="button"
        className="button small"
        disabled={!clip || phase2 === false}
        title={
          phase2 === false
            ? 'This server predates paste'
            : clip
              ? `Paste ${pasteLabel(clip)} to ${stripsLabel(session, strips)}`
              : 'Copy a strip first (its ⋮ menu)'
        }
        onClick={() => clip && void pasteClip(session, strips, clip)}
      >
        <ClipboardPaste size={13} />
        <span className="selection-paste">Paste{clip ? ` ${sectionsLabel(clip.sections)}` : ''}</span>
      </button>
      <button
        type="button"
        className="button small"
        title={`Library: apply an item to ${stripsLabel(session, strips)}`}
        onClick={() => openDialog({ kind: 'library', source: count === 1 ? strips[0] : null, targets: strips })}
      >
        <BookOpen size={13} />
        <span className="selection-library">Library</span>
      </button>
      <button
        type="button"
        className="icon-button"
        title="Clear the selection"
        aria-label="Clear the selection"
        onClick={clearSelection}
      >
        <X size={14} />
      </button>
    </div>
  )
}

function CopyDialog(props: { session: Session; strip: StripRef }) {
  const { session, strip } = props
  const [sections, setSections] = useState<Section[]>(DEFAULT_SECTIONS)
  const name = stripLabel(session, strip)
  return (
    <Modal
      small
      icon={<ClipboardCopy size={16} />}
      title={`Copy from ${name}`}
      subtitle="Kept on this device until the next copy"
      onClose={closeDialog}
      footer={
        <>
          <button type="button" className="button" onClick={closeDialog}>
            Cancel
          </button>
          <button
            type="button"
            className="button primary"
            disabled={sections.length === 0}
            onClick={() => {
              const clip = copyStrip(session, strip, sections)
              closeDialog()
              if (clip) notify(`Copied ${sectionsLabel(clip.sections)} from ${clip.source}`)
            }}
          >
            Copy {sections.length > 0 ? sectionsLabel(sections) : ''}
          </button>
        </>
      }
    >
      <SectionPicker value={sections} onChange={setSections} available={sectionsFor(strip)} />
    </Modal>
  )
}

/** Whichever workflow dialog is open (workstate.ts `openDialog`). */
export function WorkflowDialogs(props: { session: Session }) {
  const dialog = useWork((w) => w.dialog)
  const { session } = props
  // The strip a copy is from was removed: nothing left to copy.
  const gone = dialog?.kind === 'copy' && !stripExists(session, dialog.strip)
  useEffect(() => {
    if (gone) closeDialog()
  }, [gone])
  if (!dialog || gone) return null
  if (dialog.kind === 'copy') return <CopyDialog key={stripKey(dialog.strip)} session={session} strip={dialog.strip} />
  return <LibraryDialog session={session} source={dialog.source} targets={dialog.targets} />
}
