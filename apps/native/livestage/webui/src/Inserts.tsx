// An insert's window, and the picker that adds one.
//
// A built-in effect gets its editor (editors/): the native editor's layout,
// controls and live displays, drawn in the page. A third-party plug-in's
// editor is its own window on the machine that runs it, so the page offers
// bypass, order and removal only.

import { useEffect, useMemo, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import {
  ArrowDown,
  ArrowUp,
  Blend,
  CircleAlert,
  Gauge,
  Guitar,
  LoaderCircle,
  Music,
  Plug,
  Power,
  Repeat,
  Rows3,
  Search,
  SlidersHorizontal,
  Sparkles,
  Trash2,
  Waves,
  X,
} from 'lucide-react'
import { PluginEditor } from './editors/index.tsx'
import type { Command, InsertSlot, Session, StripRef } from './protocol.ts'
import { findStrip } from './routing.ts'
import { act, loadInstalled, useStore } from './store.ts'
import type { InsertTarget } from './Mixer.tsx'
import { stripLabel } from './workstate.ts'

export function categoryIcon(category: string, size = 16): ReactNode {
  switch (category) {
    case 'EQ':
      return <SlidersHorizontal size={size} />
    case 'Dynamics':
      return <Gauge size={size} />
    case 'Channel Strip':
      return <Rows3 size={size} />
    case 'Delay':
      return <Repeat size={size} />
    case 'Reverb':
      return <Waves size={size} />
    case 'Utility':
      return <Blend size={size} />
    case 'Pitch':
      return <Music size={size} />
    case 'Multi-FX':
      return <Guitar size={size} />
    default:
      return <Sparkles size={size} />
  }
}

// Open dialogs, innermost last: Escape closes only the one on top (a
// confirmation over the library closes the confirmation).
const modalStack: object[] = []

export function Modal(props: {
  icon: ReactNode
  title: string
  subtitle?: string
  onClose: () => void
  children: ReactNode
  toolbar?: ReactNode
  /** The action row under the body (Cancel and the action). */
  footer?: ReactNode
  /** A plug-in editor: wide, its own padding. */
  plugin?: boolean
  /** A confirmation or a short form. */
  small?: boolean
}) {
  const { onClose } = props
  const closeRef = useRef(onClose)
  closeRef.current = onClose
  // Once per dialog, so a re-render never moves it above one opened over it.
  useEffect(() => {
    const token = {}
    modalStack.push(token)
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && modalStack[modalStack.length - 1] === token) closeRef.current()
    }
    window.addEventListener('keydown', onKey)
    return () => {
      window.removeEventListener('keydown', onKey)
      modalStack.splice(modalStack.indexOf(token), 1)
    }
  }, [])
  return (
    <div className="modal-backdrop" onPointerDown={(e) => e.target === e.currentTarget && onClose()}>
      <div
        className={`modal${props.plugin ? ' plugin' : ''}${props.small ? ' small' : ''}`}
        role="dialog"
        aria-modal="true"
        aria-label={props.title}
      >
        <div className="modal-head">
          <span className="modal-icon">{props.icon}</span>
          <div className="modal-title">
            <span>{props.title}</span>
            {props.subtitle && <span className="modal-subtitle">{props.subtitle}</span>}
          </div>
          {props.toolbar}
          <button type="button" className="icon-button large" onClick={onClose} aria-label="Close">
            <X size={16} />
          </button>
        </div>
        <div className="modal-body">{props.children}</div>
        {props.footer && <div className="modal-foot">{props.footer}</div>}
      </div>
    </div>
  )
}

export function stripName(session: Session, strip: StripRef): string {
  return stripLabel(session, strip)
}

function stripInserts(session: Session, strip: StripRef): InsertSlot[] {
  return findStrip(session, strip)?.core.inserts ?? []
}

export function InsertEditor(props: { session: Session; target: InsertTarget; onClose: () => void }) {
  const { session, target, onClose } = props
  const hello = useStore((s) => s.hello)
  const states = useStore((s) => s.insertStates)
  const inserts = stripInserts(session, target.strip)
  const index = inserts.findIndex((slot) => slot.id === target.insert)
  const slot = inserts[index]
  // Removed (here or elsewhere): nothing left to edit.
  useEffect(() => {
    if (!slot) onClose()
  }, [slot, onClose])
  if (!slot) return null

  const plugin = slot.plugin
  const effect = plugin.type === 'builtin' ? hello?.effects.find((e) => e.stem === plugin.stem) : undefined
  const name = plugin.type === 'builtin' ? (effect?.name ?? plugin.stem) : plugin.name
  const state = states[slot.id] ?? 'ready'
  const position = `${index + 1} of ${inserts.length}`

  return (
    <Modal
      icon={plugin.type === 'builtin' ? categoryIcon(effect?.category ?? '') : <Plug size={16} />}
      title={name}
      subtitle={`${stripName(session, target.strip)} · insert ${position}`}
      onClose={onClose}
      plugin={plugin.type === 'builtin' && !!effect?.spec}
      toolbar={
        <div className="modal-tools">
          <button
            type="button"
            className={`switch${slot.bypass ? '' : ' on'}`}
            title={slot.bypass ? 'Bypassed: click to switch on' : 'Active: click to bypass'}
            onClick={() => act({ cmd: 'set_insert_bypass', insert: slot.id, bypass: !slot.bypass })}
          >
            <Power size={13} strokeWidth={2.5} />
            {slot.bypass ? 'Bypassed' : 'On'}
          </button>
          <button
            type="button"
            className="icon-button large"
            title="Move earlier"
            disabled={index <= 0}
            onClick={() => act({ cmd: 'move_insert', strip: target.strip, insert: slot.id, to: index - 1 })}
          >
            <ArrowUp size={15} />
          </button>
          <button
            type="button"
            className="icon-button large"
            title="Move later"
            disabled={index >= inserts.length - 1}
            onClick={() => act({ cmd: 'move_insert', strip: target.strip, insert: slot.id, to: index + 1 })}
          >
            <ArrowDown size={15} />
          </button>
          <button
            type="button"
            className="icon-button large danger"
            title="Remove"
            onClick={() => act({ cmd: 'remove_insert', strip: target.strip, insert: slot.id })}
          >
            <Trash2 size={15} />
          </button>
        </div>
      }
    >
      {plugin.type === 'builtin' && effect ? (
        <PluginEditor slot={slot} effect={effect} />
      ) : plugin.type === 'external' ? (
        <div className="external-card">
          <div className="external-format">{plugin.format}</div>
          <div className="external-path">{plugin.path}</div>
          {state === 'loading' && (
            <div className="external-state">
              <LoaderCircle size={14} className="spin" /> Loading in the plug-in host…
            </div>
          )}
          {typeof state === 'object' && (
            <div className="external-state error-text">
              <CircleAlert size={14} /> Did not load: {state.failed}
            </div>
          )}
          <p className="muted">
            Its editor opens on the machine that runs it, in the LiveStage desktop app. Its settings are kept in the
            session.
          </p>
        </div>
      ) : (
        <p className="muted">This effect is not in this server's build.</p>
      )}
    </Modal>
  )
}

export function EffectPicker(props: { session: Session; strip: StripRef; onClose: () => void }) {
  const hello = useStore((s) => s.hello)
  const installed = useStore((s) => s.installed)
  const [tab, setTab] = useState<'builtin' | 'installed'>('builtin')
  const [query, setQuery] = useState('')
  const external = hello?.external_plugins ?? false
  const q = query.trim().toLowerCase()

  const search = useRef<HTMLInputElement>(null)
  useEffect(() => {
    if (tab === 'installed' && installed === null) void loadInstalled()
  }, [tab, installed])
  // Typing goes to the search after switching tabs too.
  useEffect(() => search.current?.focus(), [tab])

  const add = (command: Extract<Command, { cmd: 'add_insert' }>) => {
    act(command)
    props.onClose()
  }

  const groups = useMemo(() => {
    const byCategory = new Map<string, NonNullable<typeof hello>['effects']>()
    for (const effect of hello?.effects ?? []) {
      if (q && !effect.name.toLowerCase().includes(q) && !effect.category.toLowerCase().includes(q)) continue
      const list = byCategory.get(effect.category) ?? []
      list.push(effect)
      byCategory.set(effect.category, list)
    }
    return [...byCategory]
  }, [hello, q])

  const found = useMemo(
    () =>
      (installed ?? [])
        .filter(
          (e) =>
            !q ||
            e.name.toLowerCase().includes(q) ||
            e.vendor.toLowerCase().includes(q) ||
            e.format.toLowerCase() === q,
        )
        .slice(0, 300),
    [installed, q],
  )

  return (
    <Modal
      icon={<Sparkles size={16} />}
      title="Add an effect"
      subtitle={stripName(props.session, props.strip)}
      onClose={props.onClose}
    >
      <div className="picker-bar">
        <div className="tabs">
          <button type="button" className={tab === 'builtin' ? 'on' : ''} onClick={() => setTab('builtin')}>
            Futureboard
          </button>
          {external && (
            <button type="button" className={tab === 'installed' ? 'on' : ''} onClick={() => setTab('installed')}>
              Installed
            </button>
          )}
        </div>
        <label className="search">
          <Search size={14} />
          <input
            ref={search}
            placeholder={tab === 'builtin' ? 'Search effects' : 'Search name, vendor or format'}
            value={query}
            onChange={(e) => setQuery(e.currentTarget.value)}
          />
        </label>
      </div>
      {tab === 'builtin' ? (
        <div className="picker-groups">
          {groups.length === 0 && <p className="muted">No built-in effect matches.</p>}
          {groups.map(([category, effects]) => (
            <div key={category} className="picker-group">
              <div className="picker-category">
                {categoryIcon(category, 13)} {category}
              </div>
              <div className="picker-cards">
                {effects.map((effect) => (
                  <button
                    key={effect.stem}
                    type="button"
                    className="picker-card"
                    onClick={() =>
                      add({
                        cmd: 'add_insert',
                        strip: props.strip,
                        plugin: { type: 'builtin', stem: effect.stem, params: [] },
                        index: null,
                      })
                    }
                  >
                    <span className="picker-card-icon">{categoryIcon(category, 15)}</span>
                    <span>{effect.name}</span>
                  </button>
                ))}
              </div>
            </div>
          ))}
        </div>
      ) : installed === null ? (
        <p className="muted loading-line">
          <LoaderCircle size={14} className="spin" /> Reading the plug-in catalog…
        </p>
      ) : installed.length === 0 ? (
        <p className="muted">No effects in the catalog. Scan for plug-ins in Futureboard Studio first.</p>
      ) : (
        <div className="picker-list">
          {found.length === 0 && <p className="muted">Nothing matches.</p>}
          {found.map((effect) => (
            <button
              key={`${effect.format}:${effect.path}:${effect.class_id}`}
              type="button"
              className="picker-row"
              onClick={() =>
                add({
                  cmd: 'add_insert',
                  strip: props.strip,
                  plugin: {
                    type: 'external',
                    format: effect.format,
                    path: effect.path,
                    class_id: effect.class_id,
                    name: effect.name,
                  },
                  index: null,
                })
              }
            >
              <span className="picker-row-name">{effect.name}</span>
              <span className="muted">{effect.vendor}</span>
              <span className="format-badge">{effect.format}</span>
            </button>
          ))}
        </div>
      )}
    </Modal>
  )
}
