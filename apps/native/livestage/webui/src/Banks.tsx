// The fader banks over the mixer (Phase 3): Inputs · Aux · Groups · FX ·
// Matrix · DCAs, then the show's own layers, and the layer editor (a name,
// up to any strips, in an order).

import { useRef, useState } from 'react'
import type { PointerEvent as ReactPointerEvent, ReactNode } from 'react'
import {
  ArrowDown,
  ArrowUp,
  Grid3x3,
  GripVertical,
  Layers,
  LayoutList,
  Mic,
  Pencil,
  Plus,
  SlidersVertical,
  Trash2,
} from 'lucide-react'
import { RoleBadge } from './Bus.tsx'
import { ConfirmDialog } from './Dialogs.tsx'
import { Modal } from './Inserts.tsx'
import type { Layer, Session, StripRef } from './protocol.ts'
import { LAYER_COUNT, sameStrip, stripKey } from './protocol.ts'
import type { Bank } from './routing.ts'
import { BUILTIN_BANKS, ROLES, bankStrips, findStrip, sameBank } from './routing.ts'
import { act, useStore } from './store.ts'

const BANK_ICONS: Record<string, ReactNode> = {
  inputs: <Mic size={13} />,
  aux: <Layers size={13} />,
  groups: <Layers size={13} />,
  fx: <Layers size={13} />,
  matrix: <Grid3x3 size={13} />,
  dcas: <SlidersVertical size={13} />,
}

export function BankBar(props: { session: Session; bank: Bank; onBank: (bank: Bank) => void }) {
  const { session, bank } = props
  const phase3 = useStore((s) => s.phase3)
  const [editing, setEditing] = useState<number | null>(null)
  const layer = typeof bank === 'string' ? null : bank.layer
  const full = session.layers.length >= LAYER_COUNT
  const count = (b: Bank) => (b === 'dcas' ? null : bankStrips(session, b).length)
  return (
    <div className="bank-bar">
      <div className="segments bank-segments" role="tablist" aria-label="Fader banks">
        {BUILTIN_BANKS.map(({ bank: b, label, title }) => {
          const n = count(b)
          return (
            <button
              key={b}
              type="button"
              role="tab"
              aria-selected={sameBank(bank, b)}
              className={sameBank(bank, b) ? 'on' : ''}
              title={`${title}${n === null ? '' : ` · ${n}`}`}
              onClick={() => props.onBank(b)}
            >
              <span className="bank-icon">{BANK_ICONS[b]}</span>
              <span>{label}</span>
              {n !== null && n > 0 && <span className="bank-count">{n}</span>}
            </button>
          )
        })}
        {session.layers.map((l, i) => (
          <button
            key={`layer-${i}`}
            type="button"
            role="tab"
            aria-selected={layer === i}
            className={`bank-layer${layer === i ? ' on' : ''}`}
            title={`Layer ${i + 1}: ${l.strips.length} strip${l.strips.length === 1 ? '' : 's'}. Double-click to edit`}
            onClick={() => props.onBank({ layer: i })}
            onDoubleClick={() => setEditing(i)}
          >
            <LayoutList size={13} className="bank-icon" />
            <span className="bank-layer-name">{l.name}</span>
          </button>
        ))}
      </div>
      {layer !== null && session.layers[layer] && (
        <button
          type="button"
          className="icon-button large"
          title={`Edit ${session.layers[layer].name}`}
          aria-label="Edit this layer"
          onClick={() => setEditing(layer)}
        >
          <Pencil size={14} />
        </button>
      )}
      <button
        type="button"
        className="icon-button large"
        disabled={phase3 === false || full}
        title={
          phase3 === false
            ? 'This LiveStage server predates custom layers'
            : full
              ? `${LAYER_COUNT} layers is the most a show holds`
              : 'New layer: your own bank of strips'
        }
        aria-label="New layer"
        onClick={() => setEditing(session.layers.length)}
      >
        <Plus size={15} />
      </button>
      {editing !== null && (
        <LayerEditor
          session={session}
          index={editing}
          layer={session.layers[editing] ?? null}
          onClose={() => setEditing(null)}
          onSaved={(index) => props.onBank({ layer: index })}
          onRemoved={() => props.onBank('inputs')}
        />
      )}
    </div>
  )
}

// ── The layer editor ────────────────────────────────────────────────────

interface Pickable {
  strip: StripRef
  name: string
  tag: ReactNode
}

function pickableGroups(session: Session): { title: string; items: Pickable[] }[] {
  const groups: { title: string; items: Pickable[] }[] = [
    {
      title: 'Channels',
      items: session.channels.map((c, i) => ({
        strip: { kind: 'channel', id: c.id },
        name: c.name,
        tag: <span className="layer-tag">{i + 1}</span>,
      })),
    },
    ...ROLES.map((r) => ({
      title: r.bank,
      items: session.buses
        .filter((b) => b.role === r.role)
        .map((b): Pickable => ({ strip: { kind: 'bus', id: b.id }, name: b.name, tag: <RoleBadge role={b.role} /> })),
    })),
    {
      title: 'Matrix',
      items: session.matrices.map(
        (m): Pickable => ({ strip: { kind: 'matrix', id: m.id }, name: m.name, tag: <Grid3x3 size={12} className="layer-tag-icon" /> }),
      ),
    },
  ]
  return groups.filter((g) => g.items.length > 0)
}

export function LayerEditor(props: {
  session: Session
  index: number
  layer: Layer | null
  onClose: () => void
  onSaved: (index: number) => void
  onRemoved: () => void
}) {
  const { session, index } = props
  const isNew = props.layer === null
  const [name, setName] = useState(props.layer?.name ?? `Layer ${index + 1}`)
  const [strips, setStrips] = useState<StripRef[]>(() =>
    (props.layer?.strips ?? []).filter((s) => s.kind !== 'master' && findStrip(session, s) !== null),
  )
  const [removing, setRemoving] = useState(false)
  const list = useRef<HTMLOListElement>(null)
  const [dragging, setDragging] = useState<string | null>(null)

  const has = (s: StripRef) => strips.some((t) => sameStrip(t, s))
  const toggle = (s: StripRef) => setStrips(has(s) ? strips.filter((t) => !sameStrip(t, s)) : [...strips, s])
  const move = (from: number, to: number) => {
    if (to < 0 || to >= strips.length || from === to) return
    const next = [...strips]
    const [item] = next.splice(from, 1)
    next.splice(to, 0, item)
    setStrips(next)
  }
  const ok = name.trim() !== ''
  const save = () => {
    if (!ok) return
    act({ cmd: 'set_layer', index, name: name.trim(), strips })
    props.onSaved(index)
    props.onClose()
  }

  // Drag a row by its grip: it takes the place of the row under the pointer.
  const startDrag = (e: ReactPointerEvent<HTMLElement>, key: string) => {
    if (e.button !== 0 || !list.current) return
    e.preventDefault()
    const handle = e.currentTarget
    handle.setPointerCapture(e.pointerId)
    setDragging(key)
    let order = strips
    const onMove = (ev: PointerEvent) => {
      const rows = [...(list.current?.querySelectorAll<HTMLElement>('[data-key]') ?? [])]
      const from = order.findIndex((s) => stripKey(s) === key)
      let to = rows.findIndex((row) => {
        const r = row.getBoundingClientRect()
        return ev.clientY < r.top + r.height / 2
      })
      if (to < 0) to = rows.length - 1
      else if (to > from) to -= 1
      if (from >= 0 && to !== from) {
        const next = [...order]
        const [item] = next.splice(from, 1)
        next.splice(to, 0, item)
        order = next
        setStrips(next)
      }
    }
    const onUp = () => {
      handle.removeEventListener('pointermove', onMove)
      handle.removeEventListener('pointerup', onUp)
      handle.removeEventListener('pointercancel', onUp)
      setDragging(null)
    }
    handle.addEventListener('pointermove', onMove)
    handle.addEventListener('pointerup', onUp)
    handle.addEventListener('pointercancel', onUp)
  }

  const groups = pickableGroups(session)
  const nameOf = (s: StripRef) => findStrip(session, s)?.name ?? '?'
  return (
    <>
      <Modal
        icon={<LayoutList size={16} />}
        title={isNew ? 'New layer' : `Edit ${props.layer?.name}`}
        subtitle={`Layer ${index + 1} of ${LAYER_COUNT} · saved with the show, not in scenes`}
        onClose={props.onClose}
        footer={
          <>
            {!isNew && (
              <button type="button" className="button danger" onClick={() => setRemoving(true)}>
                <Trash2 size={14} /> Delete layer
              </button>
            )}
            <span className="spacer" />
            <button type="button" className="button" onClick={props.onClose}>
              Cancel
            </button>
            <button type="button" className="button primary" disabled={!ok} onClick={save}>
              {isNew ? 'Add layer' : 'Save layer'}
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
            maxLength={24}
            onFocus={(e) => e.currentTarget.select()}
            onChange={(e) => setName(e.currentTarget.value)}
            onKeyDown={(e) => e.key === 'Enter' && save()}
          />
        </label>
        <div className="layer-editor">
          <section className="layer-pick" aria-label="Strips">
            <span className="knob-caption">Strips</span>
            {groups.length === 0 && <span className="pe-note">The show has no strips yet.</span>}
            {groups.map((g) => (
              <div key={g.title} className="layer-group">
                <span className="layer-group-title">{g.title}</span>
                <div className="layer-checks">
                  {g.items.map((item) => {
                    const on = has(item.strip)
                    return (
                      <label key={stripKey(item.strip)} className={`layer-check${on ? ' on' : ''}`}>
                        <input type="checkbox" checked={on} onChange={() => toggle(item.strip)} />
                        {item.tag}
                        <span className="layer-check-name">{item.name}</span>
                      </label>
                    )
                  })}
                </div>
              </div>
            ))}
          </section>
          <section className="layer-order" aria-label="Order">
            <span className="knob-caption">Order, left to right · {strips.length}</span>
            {strips.length === 0 ? (
              <span className="layer-empty">Tick strips to add them. A layer can mix channels, buses and matrices.</span>
            ) : (
              <ol ref={list} className="layer-list">
                {strips.map((s, i) => {
                  const key = stripKey(s)
                  return (
                    <li key={key} data-key={key} className={`layer-row${dragging === key ? ' dragging' : ''}`}>
                      <span
                        className="layer-grip"
                        role="button"
                        aria-label={`Drag ${nameOf(s)}`}
                        title="Drag to reorder"
                        onPointerDown={(e) => startDrag(e, key)}
                      >
                        <GripVertical size={13} />
                      </span>
                      <span className="layer-pos value">{i + 1}</span>
                      <span className="layer-row-name">{nameOf(s)}</span>
                      <button
                        type="button"
                        className="icon-button"
                        disabled={i === 0}
                        title="Earlier"
                        aria-label={`Move ${nameOf(s)} earlier`}
                        onClick={() => move(i, i - 1)}
                      >
                        <ArrowUp size={13} />
                      </button>
                      <button
                        type="button"
                        className="icon-button"
                        disabled={i === strips.length - 1}
                        title="Later"
                        aria-label={`Move ${nameOf(s)} later`}
                        onClick={() => move(i, i + 1)}
                      >
                        <ArrowDown size={13} />
                      </button>
                      <button
                        type="button"
                        className="icon-button danger"
                        title="Take out of the layer"
                        aria-label={`Take ${nameOf(s)} out`}
                        onClick={() => toggle(s)}
                      >
                        <Trash2 size={12} />
                      </button>
                    </li>
                  )
                })}
              </ol>
            )}
          </section>
        </div>
      </Modal>
      {removing && (
        <ConfirmDialog
          icon={<Trash2 size={16} />}
          title={`Delete ${props.layer?.name}?`}
          confirmLabel="Delete layer"
          danger
          onCancel={() => setRemoving(false)}
          onConfirm={() => {
            act({ cmd: 'remove_layer', index })
            props.onRemoved()
            props.onClose()
          }}
        >
          The layer goes; its strips stay where they are. Undo brings it back.
        </ConfirmDialog>
      )}
    </>
  )
}
