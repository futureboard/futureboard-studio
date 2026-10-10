// The page's own working state around the strips (Phase 2): which strips are
// selected (Shift/Ctrl/⌘-click, or select mode on a touch screen), the strip
// clipboard (a copy is built here from the session the page has, and kept
// in localStorage so it survives a reload), and which workflow dialog is
// open. None of it is the server's: the server only ever sees the
// `paste_strip` / `library_*` commands these lead to.

import { useSyncExternalStore } from 'react'
import { cloneProcessing } from './processing.ts'
import { findStrip, stripExists } from './routing.ts'
import type { Section, Session, StripRef, StripSettings } from './protocol.ts'
import { stripKey } from './protocol.ts'

// ── Sections ────────────────────────────────────────────────────────────

/** In the order the picker shows them, with what each carries. */
export const SECTIONS: { id: Section; label: string; detail: string }[] = [
  { id: 'processing', label: 'Processing', detail: 'HPF, gate, EQ, comp, delay and their order' },
  { id: 'hpf', label: 'HPF', detail: 'High-pass' },
  { id: 'gate', label: 'Gate', detail: 'Gate' },
  { id: 'eq', label: 'EQ', detail: 'The four bands' },
  { id: 'comp', label: 'Comp', detail: 'Compressor' },
  { id: 'delay', label: 'Delay', detail: 'Strip delay' },
  { id: 'inserts', label: 'Inserts', detail: 'The insert rack, replaced whole' },
  { id: 'sends', label: 'Sends', detail: 'Send levels and pre/post (channels)' },
  { id: 'fader_pan', label: 'Fader & Pan', detail: 'Fader level and pan' },
  { id: 'input', label: 'Input', detail: 'Trim and polarity (channels)' },
  { id: 'name_color', label: 'Name & Colour', detail: 'Strip name and colour' },
]

/** The parts of the processing section, which "processing" includes. */
export const PROCESSING_PARTS: Section[] = ['hpf', 'gate', 'eq', 'comp', 'delay']

export const DEFAULT_SECTIONS: Section[] = ['processing']

/** "EQ · Comp", in the picker's order. */
export function sectionsLabel(sections: readonly Section[]): string {
  const set = new Set(sections)
  const shown = SECTIONS.filter((s) => set.has(s.id) && !(set.has('processing') && PROCESSING_PARTS.includes(s.id)))
  return shown.length > 0 ? shown.map((s) => s.label).join(' · ') : 'nothing'
}

/** The sections a strip of this kind has. */
export function sectionsFor(strip: StripRef): Section[] {
  return SECTIONS.map((s) => s.id).filter((id) => strip.kind === 'channel' || (id !== 'sends' && id !== 'input'))
}

/** `sections` without parts "processing" already covers, in picker order. */
export function tidySections(sections: Iterable<Section>): Section[] {
  const set = new Set(sections)
  return SECTIONS.map((s) => s.id).filter(
    (id) => set.has(id) && !(set.has('processing') && PROCESSING_PARTS.includes(id)),
  )
}

// ── Strips ──────────────────────────────────────────────────────────────

/** Every strip in console order: channels, buses, matrices, the master. */
export function consoleOrder(session: Session): StripRef[] {
  return [
    ...session.channels.map((c): StripRef => ({ kind: 'channel', id: c.id })),
    ...session.buses.map((b): StripRef => ({ kind: 'bus', id: b.id })),
    ...session.matrices.map((m): StripRef => ({ kind: 'matrix', id: m.id })),
    { kind: 'master' },
  ]
}

export { stripExists }

export function stripLabel(session: Session, strip: StripRef): string {
  const found = findStrip(session, strip)
  if (found) return found.name
  return strip.kind === 'bus' ? 'Bus' : strip.kind === 'matrix' ? 'Matrix' : 'Channel'
}

/** "Kick", "Kick and Snare", "Kick, Snare and 2 more". */
export function stripsLabel(session: Session, strips: StripRef[]): string {
  const names = strips.map((s) => stripLabel(session, s))
  if (names.length <= 2) return names.join(' and ')
  if (names.length === 3) return `${names[0]}, ${names[1]} and ${names[2]}`
  return `${names[0]}, ${names[1]} and ${names.length - 2} more`
}

/** What a copy of `sections` from `strip` carries, from the session as the
 *  page has it. Parts a strip of this kind lacks are left out. */
export function buildSettings(session: Session, strip: StripRef, sections: readonly Section[]): StripSettings | null {
  const found = findStrip(session, strip)
  if (!found) return null
  const { core, channel } = found
  const set = new Set(sections)
  const out: StripSettings = {}
  if (set.has('processing') || PROCESSING_PARTS.some((p) => set.has(p))) out.processing = cloneProcessing(core.processing)
  if (set.has('inserts')) {
    out.inserts = core.inserts.map((slot) => ({ bypass: slot.bypass, plugin: structuredClone(slot.plugin) }))
  }
  if (set.has('sends') && channel) out.sends = channel.sends.map((s) => ({ ...s }))
  if (set.has('fader_pan')) {
    out.fader_db = core.fader_db
    out.pan = core.pan
  }
  if (set.has('input') && channel) {
    out.trim_db = channel.trim_db
    out.phase_invert = channel.phase_invert
  }
  if (set.has('name_color')) {
    if (strip.kind !== 'master') out.name = found.name
    out.color = core.color
  }
  return out
}

// ── A tiny external store ───────────────────────────────────────────────

export interface Clip {
  /** Where it was copied from, for the labels ("from Kick"). */
  source: string
  sections: Section[]
  settings: StripSettings
  at: number
}

export type Dialog =
  | { kind: 'copy'; strip: StripRef }
  | { kind: 'library'; source: StripRef | null; targets: StripRef[] }
  | null

interface WorkState {
  /** Selected strips, by `stripKey`, in the order they were picked. */
  selection: string[]
  /** Where a Shift-click range starts. */
  anchor: string | null
  /** A tap on a strip's name selects it instead of opening it (touch). */
  selectMode: boolean
  clip: Clip | null
  dialog: Dialog
}

const CLIP_KEY = 'livestage.clipboard'

function loadClip(): Clip | null {
  try {
    const raw = localStorage.getItem(CLIP_KEY)
    if (!raw) return null
    const clip = JSON.parse(raw) as Clip
    return clip && Array.isArray(clip.sections) && typeof clip.settings === 'object' ? clip : null
  } catch {
    return null
  }
}

function saveClip(clip: Clip | null) {
  try {
    if (clip) localStorage.setItem(CLIP_KEY, JSON.stringify(clip))
    else localStorage.removeItem(CLIP_KEY)
  } catch {
    // A private window or blocked storage: the clipboard lasts this page only.
  }
}

let work: WorkState = { selection: [], anchor: null, selectMode: false, clip: loadClip(), dialog: null }
const listeners = new Set<() => void>()

function setWork(patch: Partial<WorkState>) {
  work = { ...work, ...patch }
  for (const listener of listeners) listener()
}

function subscribe(listener: () => void) {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

/** `selector` must return something already in the state (or a primitive). */
export function useWork<T>(selector: (state: WorkState) => T): T {
  return useSyncExternalStore(subscribe, () => selector(work))
}

export function getWork(): WorkState {
  return work
}

// ── Selection ───────────────────────────────────────────────────────────

export function refFromKey(key: string): StripRef | null {
  if (key === 'master') return { kind: 'master' }
  const [kind, id] = key.split(':')
  return (kind === 'channel' || kind === 'bus' || kind === 'matrix') && id ? { kind, id: Number(id) } : null
}

/** The selected strips that still exist, in console order. */
export function selectedStrips(session: Session, selection = work.selection): StripRef[] {
  const set = new Set(selection)
  return consoleOrder(session).filter((s) => set.has(stripKey(s)))
}

export function setSelectMode(on: boolean) {
  setWork({ selectMode: on })
}

export function clearSelection() {
  setWork({ selection: [], anchor: null })
}

export function setSelection(strips: StripRef[]) {
  const keys = strips.map(stripKey)
  setWork({ selection: keys, anchor: keys[keys.length - 1] ?? null })
}

/** A click on a strip's header with a modifier (or in select mode): Ctrl/⌘
 *  or a tap toggles it, Shift extends from the last one picked through the
 *  strips shown. */
export function pick(strip: StripRef, range: boolean, shown: StripRef[]) {
  const key = stripKey(strip)
  const { selection, anchor } = work
  if (range && anchor) {
    const keys = shown.map(stripKey)
    const from = keys.indexOf(anchor)
    const to = keys.indexOf(key)
    if (from >= 0 && to >= 0) {
      const [lo, hi] = from < to ? [from, to] : [to, from]
      const add = keys.slice(lo, hi + 1)
      setWork({ selection: [...selection.filter((k) => !add.includes(k)), ...add], anchor: key })
      return
    }
  }
  setWork({
    selection: selection.includes(key) ? selection.filter((k) => k !== key) : [...selection, key],
    anchor: key,
  })
}

/** Whether a click on a strip's header picks rather than opens. */
export function picking(e: { shiftKey: boolean; ctrlKey: boolean; metaKey: boolean }): boolean {
  return work.selectMode || e.shiftKey || e.ctrlKey || e.metaKey
}

// ── Clipboard ───────────────────────────────────────────────────────────

export function copyStrip(session: Session, strip: StripRef, sections: Section[]): Clip | null {
  const tidy = tidySections(sections)
  const settings = buildSettings(session, strip, tidy)
  if (!settings || tidy.length === 0) return null
  const clip: Clip = { source: stripLabel(session, strip), sections: tidy, settings, at: Date.now() }
  saveClip(clip)
  setWork({ clip })
  return clip
}

export function clearClip() {
  saveClip(null)
  setWork({ clip: null })
}

// ── Dialogs ─────────────────────────────────────────────────────────────

export function openDialog(dialog: Dialog) {
  setWork({ dialog })
}

export function closeDialog() {
  setWork({ dialog: null })
}

/** The strips a strip's own menu acts on: the selection when the strip is
 *  part of it, else the strip alone. */
export function targetsFor(session: Session, strip: StripRef): StripRef[] {
  const selection = selectedStrips(session)
  return selection.some((s) => stripKey(s) === stripKey(strip)) ? selection : [strip]
}
