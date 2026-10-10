// The strip library (Phase 2): starting points kept by the server across
// shows. Browse by category (factory items are marked and read-only), apply
// an item to the selection or the strip it was opened from, save a strip's
// sections as a new item, rename or delete your own.

import { useMemo, useState } from 'react'
import { BookOpen, Lock, Pencil, Save, Trash2 } from 'lucide-react'
import { ConfirmDialog } from './Dialogs.tsx'
import { Modal } from './Inserts.tsx'
import type { LibraryItem, Section, Session, StripRef } from './protocol.ts'
import { explain, notify, request, useStore } from './store.ts'
import { SectionPicker } from './Sections.tsx'
import {
  DEFAULT_SECTIONS,
  buildSettings,
  closeDialog,
  sectionsFor,
  sectionsLabel,
  stripExists,
  stripLabel,
  stripsLabel,
  tidySections,
} from './workstate.ts'

type Tab = 'browse' | 'save'
const ALL = '\u0000all'

export function LibraryDialog(props: { session: Session; source: StripRef | null; targets: StripRef[] }) {
  const { session } = props
  const library = useStore((s) => s.library)
  const phase2 = useStore((s) => s.phase2)
  const source = props.source && stripExists(session, props.source) ? props.source : null
  const targets = props.targets.filter((t) => stripExists(session, t))
  const [tab, setTab] = useState<Tab>('browse')
  const [category, setCategory] = useState<string>(ALL)

  const categories = useMemo(() => {
    const seen = new Map<string, boolean>()
    for (const item of library ?? []) {
      const name = item.category || 'Other'
      seen.set(name, (seen.get(name) ?? false) || !item.factory)
    }
    return [...seen.keys()].sort((a, b) => a.localeCompare(b))
  }, [library])

  const subtitle =
    tab === 'save'
      ? source
        ? `Save ${stripLabel(session, source)} as an item`
        : 'Select one strip to save it'
      : targets.length > 0
        ? `Apply to ${stripsLabel(session, targets)}`
        : 'Select strips in the mixer to apply an item'

  let body
  if (library === null) {
    body = (
      <div className="card-empty">
        {phase2 === false
          ? 'This LiveStage server predates the strip library. Update the server to keep strip settings across shows.'
          : 'The server has not sent its library. It may have been started without one.'}
      </div>
    )
  } else if (tab === 'save') {
    body = (
      <SaveForm
        session={session}
        source={source}
        categories={categories}
        onSaved={(saved) => {
          setCategory(saved)
          setTab('browse')
        }}
      />
    )
  } else {
    const shown = (category === ALL ? library : library.filter((i) => (i.category || 'Other') === category))
      .slice()
      .sort((a, b) => a.category.localeCompare(b.category) || a.name.localeCompare(b.name))
    body = (
      <>
        <div className="lib-categories" role="tablist" aria-label="Categories">
          {[ALL, ...categories].map((c) => (
            <button
              key={c}
              type="button"
              role="tab"
              aria-selected={category === c}
              className={`lib-category${category === c ? ' on' : ''}`}
              onClick={() => setCategory(c)}
            >
              {c === ALL ? `All · ${library.length}` : c}
            </button>
          ))}
        </div>
        {shown.length === 0 ? (
          <div className="card-empty">Nothing here yet. Save a strip to start.</div>
        ) : (
          <div className="lib-list">
            {shown.map((item) => (
              <ItemRow key={item.id} item={item} session={session} targets={targets} categories={categories} />
            ))}
          </div>
        )}
      </>
    )
  }

  return (
    <Modal
      icon={<BookOpen size={16} />}
      title="Library"
      subtitle={subtitle}
      onClose={closeDialog}
      toolbar={
        library !== null && (
          <div className="segments lib-tabs" role="tablist">
            <button type="button" role="tab" aria-selected={tab === 'browse'} className={tab === 'browse' ? 'on' : ''} onClick={() => setTab('browse')}>
              Browse
            </button>
            <button type="button" role="tab" aria-selected={tab === 'save'} className={tab === 'save' ? 'on' : ''} onClick={() => setTab('save')}>
              <Save size={13} /> Save
            </button>
          </div>
        )
      }
    >
      {body}
    </Modal>
  )
}

function ItemRow(props: { item: LibraryItem; session: Session; targets: StripRef[]; categories: string[] }) {
  const { item, session, targets } = props
  const [busy, setBusy] = useState(false)
  const [renaming, setRenaming] = useState(false)
  const [deleting, setDeleting] = useState(false)
  const apply = async () => {
    setBusy(true)
    const reply = await request({ cmd: 'library_apply', item: item.id, targets })
    setBusy(false)
    if (reply.ok) {
      notify(`Applied “${item.name}” (${sectionsLabel(item.sections)}) to ${stripsLabel(session, targets)}`)
      closeDialog()
    } else {
      notify(explain(reply.error), true)
    }
  }
  return (
    <div className={`lib-item${item.factory ? ' factory' : ''}`}>
      <div className="lib-item-main">
        <span className="lib-item-name">
          {item.name}
          {item.factory && (
            <span className="lib-factory" title="Ships with LiveStage: read-only">
              <Lock size={10} /> Factory
            </span>
          )}
        </span>
        <span className="lib-item-meta">
          <span className="lib-item-category">{item.category || 'Other'}</span>
          <span className="lib-item-sections">{sectionsLabel(item.sections)}</span>
        </span>
      </div>
      <div className="lib-item-actions">
        {!item.factory && (
          <>
            <button type="button" className="icon-button large" title="Rename or move to another category" aria-label={`Rename ${item.name}`} onClick={() => setRenaming(true)}>
              <Pencil size={14} />
            </button>
            <button type="button" className="icon-button large danger" title="Delete from the library" aria-label={`Delete ${item.name}`} onClick={() => setDeleting(true)}>
              <Trash2 size={14} />
            </button>
          </>
        )}
        <button
          type="button"
          className="button small"
          disabled={targets.length === 0 || busy}
          title={targets.length === 0 ? 'Select strips in the mixer first' : `Apply to ${stripsLabel(session, targets)}`}
          onClick={() => void apply()}
        >
          Apply
        </button>
      </div>
      {renaming && <RenameDialog item={item} categories={props.categories} onClose={() => setRenaming(false)} />}
      {deleting && (
        <ConfirmDialog
          icon={<Trash2 size={16} />}
          title={`Delete “${item.name}”?`}
          confirmLabel="Delete"
          danger
          onCancel={() => setDeleting(false)}
          onConfirm={() => {
            void request({ cmd: 'library_delete', item: item.id }).then((reply) => {
              if (!reply.ok) notify(explain(reply.error), true)
            })
          }}
        >
          It goes from the library on this LiveStage for good. Strips it was applied to keep their settings.
        </ConfirmDialog>
      )}
    </div>
  )
}

function RenameDialog(props: { item: LibraryItem; categories: string[]; onClose: () => void }) {
  const [name, setName] = useState(props.item.name)
  const [category, setCategory] = useState(props.item.category)
  const ok = name.trim() !== ''
  const submit = () => {
    if (!ok) return
    void request({ cmd: 'library_rename', item: props.item.id, name: name.trim(), category: category.trim() }).then(
      (reply) => {
        if (!reply.ok) notify(explain(reply.error), true)
      },
    )
    props.onClose()
  }
  return (
    <Modal
      small
      icon={<Pencil size={16} />}
      title="Rename library item"
      onClose={props.onClose}
      footer={
        <>
          <button type="button" className="button" onClick={props.onClose}>
            Cancel
          </button>
          <button type="button" className="button primary" disabled={!ok} onClick={submit}>
            Rename
          </button>
        </>
      }
    >
      <label className="dialog-field">
        <span>Name</span>
        <input className="text-input" autoFocus value={name} onChange={(e) => setName(e.currentTarget.value)} onKeyDown={(e) => e.key === 'Enter' && submit()} />
      </label>
      <CategoryField value={category} onChange={setCategory} categories={props.categories} />
    </Modal>
  )
}

function CategoryField(props: { value: string; onChange: (value: string) => void; categories: string[] }) {
  return (
    <div className="dialog-field">
      <label className="dialog-field">
        <span>Category</span>
        <input
          className="text-input"
          value={props.value}
          list="lib-category-list"
          placeholder="Vocal, Kick, Keys…"
          onChange={(e) => props.onChange(e.currentTarget.value)}
        />
      </label>
      <datalist id="lib-category-list">
        {props.categories.map((c) => (
          <option key={c} value={c} />
        ))}
      </datalist>
      {props.categories.length > 0 && (
        <div className="lib-category-picks">
          {props.categories.map((c) => (
            <button key={c} type="button" className={`lib-category small${props.value === c ? ' on' : ''}`} onClick={() => props.onChange(c)}>
              {c}
            </button>
          ))}
        </div>
      )}
    </div>
  )
}

function SaveForm(props: {
  session: Session
  source: StripRef | null
  categories: string[]
  onSaved: (category: string) => void
}) {
  const { session, source } = props
  const [name, setName] = useState(source ? stripLabel(session, source) : '')
  const [category, setCategory] = useState('')
  const [sections, setSections] = useState<Section[]>(DEFAULT_SECTIONS)
  const [busy, setBusy] = useState(false)
  if (!source) {
    return (
      <div className="card-empty">
        Select one strip in the mixer (or open its Selected Channel) to save its settings to the library.
      </div>
    )
  }
  const ok = name.trim() !== '' && sections.length > 0
  const save = async () => {
    const tidy = tidySections(sections)
    const settings = buildSettings(session, source, tidy)
    if (!settings) return
    setBusy(true)
    const reply = await request({
      cmd: 'library_save',
      name: name.trim(),
      category: category.trim() || 'Other',
      settings,
      sections: tidy,
    })
    setBusy(false)
    if (reply.ok) {
      notify(`Saved “${name.trim()}” (${sectionsLabel(tidy)}) to the library`)
      props.onSaved(category.trim() || 'Other')
    } else {
      notify(explain(reply.error), true)
    }
  }
  return (
    <div className="lib-save">
      <span className="dialog-hint">
        From <strong>{stripLabel(session, source)}</strong>, as it is now.
      </span>
      <label className="dialog-field">
        <span>Name</span>
        <input className="text-input" value={name} onChange={(e) => setName(e.currentTarget.value)} placeholder="Lead vocal" />
      </label>
      <CategoryField value={category} onChange={setCategory} categories={props.categories} />
      <div className="dialog-field">
        <span>Sections</span>
        <SectionPicker value={sections} onChange={setSections} available={sectionsFor(source)} />
      </div>
      <div className="lib-save-actions">
        <button type="button" className="button primary" disabled={!ok || busy} onClick={() => void save()}>
          <Save size={14} /> Save {sections.length > 0 ? sectionsLabel(sections) : ''}
        </button>
      </div>
    </div>
  )
}
